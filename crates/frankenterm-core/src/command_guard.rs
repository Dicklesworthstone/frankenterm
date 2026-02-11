//! Native command guard — per-pane destructive command blocking.
//!
//! Replaces the dcg subprocess call with in-process Aho-Corasick + regex
//! pattern matching. Provides per-pane trust levels and a centralized policy
//! engine with audit logging.
//!
//! # Architecture
//!
//! ```text
//! Command text
//!      │
//!      ▼
//! Quick Reject (Aho-Corasick keyword scan, O(n))
//!      │ keyword hit
//!      ▼
//! Safe Patterns (regex whitelist, checked first)
//!      │ no safe match
//!      ▼
//! Destructive Patterns (regex blacklist)
//!      │ match found
//!      ▼
//! Trust Level → Block / Warn / Allow
//! ```
//!
//! # Performance
//!
//! - Quick reject: 99%+ of benign commands exit in O(n) via Aho-Corasick
//! - Safe patterns: regex whitelist short-circuits before destructive scan
//! - Pattern matching target: <100μs per command
//! - Policy lookup: <10μs via HashMap

use aho_corasick::AhoCorasick;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Instant;

// ============================================================================
// Trust Levels
// ============================================================================

/// Per-pane trust level controlling how strictly commands are guarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    /// Agent panes: block all destructive operations by default.
    Strict,
    /// Human panes: warn but allow with confirmation.
    Permissive,
    /// Monitoring/read-only panes: no command interception.
    ReadOnly,
}

impl Default for TrustLevel {
    fn default() -> Self {
        Self::Strict
    }
}

impl std::fmt::Display for TrustLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Strict => write!(f, "strict"),
            Self::Permissive => write!(f, "permissive"),
            Self::ReadOnly => write!(f, "read_only"),
        }
    }
}

// ============================================================================
// Guard Decision
// ============================================================================

/// Result of evaluating a command through the guard.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum GuardDecision {
    /// Command is safe to execute.
    Allow,
    /// Command blocked — destructive pattern matched.
    Block {
        rule_id: String,
        pack: String,
        reason: String,
        suggestions: Vec<String>,
    },
    /// Command is destructive but trust level permits with warning.
    Warn {
        rule_id: String,
        pack: String,
        reason: String,
    },
}

impl GuardDecision {
    #[must_use]
    pub fn is_blocked(&self) -> bool {
        matches!(self, Self::Block { .. })
    }

    #[must_use]
    pub fn is_warning(&self) -> bool {
        matches!(self, Self::Warn { .. })
    }

    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }

    #[must_use]
    pub fn rule_id(&self) -> Option<&str> {
        match self {
            Self::Block { rule_id, .. } | Self::Warn { rule_id, .. } => Some(rule_id),
            Self::Allow => None,
        }
    }
}

// ============================================================================
// Security Packs
// ============================================================================

/// A single destructive pattern rule.
struct DestructiveRule {
    id: &'static str,
    pattern: &'static LazyLock<Regex>,
    reason: &'static str,
    suggestions: &'static [&'static str],
}

/// A single safe pattern (whitelist).
struct SafeRule {
    pattern: &'static LazyLock<Regex>,
}

/// A security pack — collection of keyword triggers, safe patterns, and
/// destructive patterns for a specific domain.
struct SecurityPack {
    id: &'static str,
    keywords: &'static [&'static str],
    safe_rules: &'static [SafeRule],
    destructive_rules: &'static [DestructiveRule],
}

// ---------------------------------------------------------------------------
// Pack: core.filesystem
// ---------------------------------------------------------------------------

static RM_RF_ROOT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\brm\s+(-[a-z]*r[a-z]*\s+(-[a-z]*f[a-z]*\s+)?|-[a-z]*f[a-z]*\s+(-[a-z]*r[a-z]*\s+)?)\s*(/\s*$|~\s*$|\$HOME\s*$)").unwrap()
});
static RM_RF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\brm\s+(-[a-z]*r[a-z]*\s+(-[a-z]*f[a-z]*\s+)?|-[a-z]*f[a-z]*\s+(-[a-z]*r[a-z]*\s+)?)",
    )
    .unwrap()
});
static RM_RF_SAFE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\brm\s+(-[a-z]*r[a-z]*f?[a-z]*\s+)(node_modules|target|__pycache__|\.cache|dist|build|\.next|\.turbo|tmp)\b").unwrap()
});
static CHMOD_RECURSIVE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bchmod\s+(-[a-z]*R[a-z]*\s+)?(777|666|000)\s").unwrap());
static DD_OF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bdd\b.*\bof=\s*/dev/(sd[a-z]|nvme|disk|hd[a-z])").unwrap());
static MKFS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(mkfs|mke2fs)\b").unwrap());

static PACK_FILESYSTEM: SecurityPack = SecurityPack {
    id: "core.filesystem",
    keywords: &["rm", "chmod", "dd", "mkfs", "mke2fs", "shred"],
    safe_rules: &[SafeRule {
        pattern: &RM_RF_SAFE,
    }],
    destructive_rules: &[
        DestructiveRule {
            id: "core.filesystem:rm-rf-root",
            pattern: &RM_RF_ROOT,
            reason: "rm -rf targeting root/home — catastrophic data loss",
            suggestions: &["Use a specific subdirectory path instead"],
        },
        DestructiveRule {
            id: "core.filesystem:rm-rf",
            pattern: &RM_RF,
            reason: "Recursive forced deletion — verify target path",
            suggestions: &["Add --interactive or use trash-put instead"],
        },
        DestructiveRule {
            id: "core.filesystem:chmod-recursive-wide",
            pattern: &CHMOD_RECURSIVE,
            reason: "Recursive chmod to wide-open permissions",
            suggestions: &["Use more restrictive permissions (755 or 644)"],
        },
        DestructiveRule {
            id: "core.filesystem:dd-device",
            pattern: &DD_OF,
            reason: "dd writing directly to block device — data loss",
            suggestions: &["Double-check the of= target device"],
        },
        DestructiveRule {
            id: "core.filesystem:mkfs",
            pattern: &MKFS,
            reason: "Filesystem creation destroys existing data",
            suggestions: &["Verify the target partition is correct"],
        },
    ],
};

// ---------------------------------------------------------------------------
// Pack: core.git
// ---------------------------------------------------------------------------

static GIT_PUSH_FORCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bgit\s+push\b.*(\s--force\b|\s-f\b)").unwrap());
static GIT_PUSH_FORCE_LEASE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bgit\s+push\b.*--force-with-lease\b").unwrap());
static GIT_RESET_HARD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bgit\s+reset\s+--hard\b").unwrap());
static GIT_CLEAN_FD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bgit\s+clean\b.*(-[a-z]*f[a-z]*d|-[a-z]*d[a-z]*f)").unwrap()
});
static GIT_BRANCH_DELETE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bgit\s+branch\s+-D\b").unwrap());
static GIT_CHECKOUT_DOT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bgit\s+checkout\s+--\s*\.\s*$").unwrap());
static GIT_STASH_DROP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bgit\s+stash\s+(drop|clear)\b").unwrap());
static GIT_REBASE_FORCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bgit\s+rebase\b.*\b(--force-rebase|--root)\b").unwrap());

static PACK_GIT: SecurityPack = SecurityPack {
    id: "core.git",
    keywords: &["git"],
    safe_rules: &[SafeRule {
        pattern: &GIT_PUSH_FORCE_LEASE,
    }],
    destructive_rules: &[
        DestructiveRule {
            id: "core.git:push-force",
            pattern: &GIT_PUSH_FORCE,
            reason: "Force push rewrites remote history",
            suggestions: &["Use --force-with-lease for safer force push"],
        },
        DestructiveRule {
            id: "core.git:reset-hard",
            pattern: &GIT_RESET_HARD,
            reason: "git reset --hard discards uncommitted changes",
            suggestions: &["Use git stash to save changes first"],
        },
        DestructiveRule {
            id: "core.git:clean-fd",
            pattern: &GIT_CLEAN_FD,
            reason: "git clean removes untracked files permanently",
            suggestions: &["Use git clean -n for dry run first"],
        },
        DestructiveRule {
            id: "core.git:branch-delete-force",
            pattern: &GIT_BRANCH_DELETE,
            reason: "git branch -D force-deletes without merge check",
            suggestions: &["Use -d (lowercase) for safe delete with merge check"],
        },
        DestructiveRule {
            id: "core.git:checkout-dot",
            pattern: &GIT_CHECKOUT_DOT,
            reason: "git checkout -- . discards all unstaged changes",
            suggestions: &["Use git stash to save changes first"],
        },
        DestructiveRule {
            id: "core.git:stash-drop",
            pattern: &GIT_STASH_DROP,
            reason: "Stash drop/clear permanently deletes stashed changes",
            suggestions: &["Use git stash list to review before dropping"],
        },
        DestructiveRule {
            id: "core.git:rebase-force",
            pattern: &GIT_REBASE_FORCE,
            reason: "Forced rebase rewrites commit history",
            suggestions: &["Ensure no shared commits will be affected"],
        },
    ],
};

// ---------------------------------------------------------------------------
// Pack: database
// ---------------------------------------------------------------------------

static SQL_DROP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(DROP\s+(TABLE|DATABASE|SCHEMA|INDEX|VIEW|TRIGGER|FUNCTION|PROCEDURE))\b")
        .unwrap()
});
static SQL_TRUNCATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bTRUNCATE\s+(TABLE\s+)?\w").unwrap());
static SQL_DELETE_NO_WHERE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bDELETE\s+FROM\s+\w+\s*;").unwrap());
static SQL_ALTER_DROP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bALTER\s+TABLE\s+\w+\s+DROP\s+(COLUMN|CONSTRAINT|INDEX)\b").unwrap()
});

static PACK_DATABASE: SecurityPack = SecurityPack {
    id: "database",
    keywords: &[
        "DROP",
        "drop",
        "TRUNCATE",
        "truncate",
        "DELETE",
        "delete",
        "ALTER",
        "alter",
        "psql",
        "mysql",
        "sqlite3",
        "mongosh",
        "redis-cli",
    ],
    safe_rules: &[],
    destructive_rules: &[
        DestructiveRule {
            id: "database:drop",
            pattern: &SQL_DROP,
            reason: "DROP permanently destroys database objects",
            suggestions: &["Use IF EXISTS and take a backup first"],
        },
        DestructiveRule {
            id: "database:truncate",
            pattern: &SQL_TRUNCATE,
            reason: "TRUNCATE removes all rows without logging",
            suggestions: &["Use DELETE with WHERE for selective removal"],
        },
        DestructiveRule {
            id: "database:delete-no-where",
            pattern: &SQL_DELETE_NO_WHERE,
            reason: "DELETE without WHERE clause removes all rows",
            suggestions: &["Add a WHERE clause to limit deletion scope"],
        },
        DestructiveRule {
            id: "database:alter-drop",
            pattern: &SQL_ALTER_DROP,
            reason: "ALTER TABLE DROP permanently removes columns/constraints",
            suggestions: &["Back up the table before altering"],
        },
    ],
};

// ---------------------------------------------------------------------------
// Pack: containers
// ---------------------------------------------------------------------------

static DOCKER_SYSTEM_PRUNE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bdocker\s+system\s+prune\b").unwrap());
static DOCKER_RM_FORCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bdocker\s+(rm|rmi)\s+(-[a-z]*f[a-z]*\s+)?").unwrap());
static DOCKER_VOLUME_RM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bdocker\s+volume\s+(rm|prune)\b").unwrap());

static PACK_CONTAINERS: SecurityPack = SecurityPack {
    id: "containers",
    keywords: &["docker", "podman", "docker-compose"],
    safe_rules: &[],
    destructive_rules: &[
        DestructiveRule {
            id: "containers:system-prune",
            pattern: &DOCKER_SYSTEM_PRUNE,
            reason: "docker system prune removes all unused data",
            suggestions: &["Use --filter to limit what gets pruned"],
        },
        DestructiveRule {
            id: "containers:rm-force",
            pattern: &DOCKER_RM_FORCE,
            reason: "Force removing containers/images",
            suggestions: &["Stop containers gracefully first"],
        },
        DestructiveRule {
            id: "containers:volume-rm",
            pattern: &DOCKER_VOLUME_RM,
            reason: "Removing Docker volumes destroys persistent data",
            suggestions: &["Back up volume data before removal"],
        },
    ],
};

// ---------------------------------------------------------------------------
// Pack: kubernetes
// ---------------------------------------------------------------------------

static KUBECTL_DELETE_NS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bkubectl\s+delete\s+(namespace|ns)\b").unwrap());
static KUBECTL_DELETE_ALL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bkubectl\s+delete\b.*--all\b").unwrap());
static KUBECTL_DRAIN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bkubectl\s+drain\b").unwrap());
static HELM_UNINSTALL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bhelm\s+(uninstall|delete)\b").unwrap());

static PACK_KUBERNETES: SecurityPack = SecurityPack {
    id: "kubernetes",
    keywords: &["kubectl", "helm", "kustomize"],
    safe_rules: &[],
    destructive_rules: &[
        DestructiveRule {
            id: "kubernetes:delete-namespace",
            pattern: &KUBECTL_DELETE_NS,
            reason: "Deleting a namespace removes all resources within it",
            suggestions: &["Verify the namespace and its contents first"],
        },
        DestructiveRule {
            id: "kubernetes:delete-all",
            pattern: &KUBECTL_DELETE_ALL,
            reason: "kubectl delete --all removes all resources of that type",
            suggestions: &["Use label selectors for targeted deletion"],
        },
        DestructiveRule {
            id: "kubernetes:drain",
            pattern: &KUBECTL_DRAIN,
            reason: "Node drain evicts all pods",
            suggestions: &["Use --dry-run first, ensure PDBs are configured"],
        },
        DestructiveRule {
            id: "kubernetes:helm-uninstall",
            pattern: &HELM_UNINSTALL,
            reason: "Helm uninstall removes the release and its resources",
            suggestions: &["Use --keep-history to allow rollback"],
        },
    ],
};

// ---------------------------------------------------------------------------
// Pack: cloud (AWS/GCP/Azure)
// ---------------------------------------------------------------------------

static AWS_S3_RM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\baws\s+s3\s+(rm|rb)\b.*--recursive\b").unwrap());
static AWS_EC2_TERMINATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\baws\s+ec2\s+terminate-instances\b").unwrap());
static TERRAFORM_DESTROY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bterraform\s+destroy\b").unwrap());

static PACK_CLOUD: SecurityPack = SecurityPack {
    id: "cloud",
    keywords: &["aws", "gcloud", "az", "terraform", "pulumi"],
    safe_rules: &[],
    destructive_rules: &[
        DestructiveRule {
            id: "cloud:aws-s3-rm-recursive",
            pattern: &AWS_S3_RM,
            reason: "Recursive S3 deletion can destroy entire buckets",
            suggestions: &["Use --dryrun first to preview deletions"],
        },
        DestructiveRule {
            id: "cloud:aws-ec2-terminate",
            pattern: &AWS_EC2_TERMINATE,
            reason: "Terminating EC2 instances is irreversible",
            suggestions: &["Use stop-instances for recoverable shutdown"],
        },
        DestructiveRule {
            id: "cloud:terraform-destroy",
            pattern: &TERRAFORM_DESTROY,
            reason: "terraform destroy removes all managed infrastructure",
            suggestions: &["Use -target for selective destruction, or plan first"],
        },
    ],
};

// ---------------------------------------------------------------------------
// Pack: system
// ---------------------------------------------------------------------------

static KILL_MINUS_9: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(kill\s+-9|kill\s+-KILL|killall\s+-9)\b").unwrap());
static SYSTEMCTL_STOP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(systemctl|service)\s+(stop|disable|mask)\b").unwrap());
static REBOOT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(reboot|shutdown|halt|poweroff|init\s+[06])\b").unwrap());

static PACK_SYSTEM: SecurityPack = SecurityPack {
    id: "system",
    keywords: &[
        "kill",
        "killall",
        "pkill",
        "systemctl",
        "service",
        "reboot",
        "shutdown",
        "halt",
        "poweroff",
        "init",
    ],
    safe_rules: &[],
    destructive_rules: &[
        DestructiveRule {
            id: "system:kill-9",
            pattern: &KILL_MINUS_9,
            reason: "SIGKILL does not allow graceful shutdown",
            suggestions: &["Use SIGTERM (kill -15) first for graceful shutdown"],
        },
        DestructiveRule {
            id: "system:service-stop",
            pattern: &SYSTEMCTL_STOP,
            reason: "Stopping/disabling system services can break functionality",
            suggestions: &["Verify the service is not critical before stopping"],
        },
        DestructiveRule {
            id: "system:reboot",
            pattern: &REBOOT,
            reason: "System reboot/shutdown affects all running processes",
            suggestions: &["Save all work and notify users before rebooting"],
        },
    ],
};

// ---------------------------------------------------------------------------
// Pack: package_managers
// ---------------------------------------------------------------------------

static NPM_UNPUBLISH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bnpm\s+unpublish\b").unwrap());
static CARGO_YANK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bcargo\s+yank\b").unwrap());
static PIP_UNINSTALL_ALL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bpip3?\s+uninstall\b.*-y\b").unwrap());

static PACK_PACKAGE_MANAGERS: SecurityPack = SecurityPack {
    id: "package_managers",
    keywords: &["npm", "cargo", "pip", "pip3", "gem", "brew"],
    safe_rules: &[],
    destructive_rules: &[
        DestructiveRule {
            id: "package_managers:npm-unpublish",
            pattern: &NPM_UNPUBLISH,
            reason: "npm unpublish removes packages from the registry",
            suggestions: &["Use npm deprecate instead"],
        },
        DestructiveRule {
            id: "package_managers:cargo-yank",
            pattern: &CARGO_YANK,
            reason: "cargo yank prevents new downloads of a version",
            suggestions: &["Publish a patched version instead"],
        },
        DestructiveRule {
            id: "package_managers:pip-uninstall-all",
            pattern: &PIP_UNINSTALL_ALL,
            reason: "Bulk pip uninstall can break the environment",
            suggestions: &["Uninstall specific packages instead"],
        },
    ],
};

// ---------------------------------------------------------------------------
// All packs
// ---------------------------------------------------------------------------

static ALL_PACKS: &[&SecurityPack] = &[
    &PACK_FILESYSTEM,
    &PACK_GIT,
    &PACK_DATABASE,
    &PACK_CONTAINERS,
    &PACK_KUBERNETES,
    &PACK_CLOUD,
    &PACK_SYSTEM,
    &PACK_PACKAGE_MANAGERS,
];

/// Build an Aho-Corasick automaton from all pack keywords.
static KEYWORD_AUTOMATON: LazyLock<AhoCorasick> = LazyLock::new(|| {
    let mut keywords = Vec::new();
    for pack in ALL_PACKS {
        for kw in pack.keywords {
            keywords.push(*kw);
        }
    }
    AhoCorasick::builder()
        .ascii_case_insensitive(true)
        .build(&keywords)
        .expect("valid Aho-Corasick patterns")
});

/// Map keyword → pack IDs for quick-reject filtering.
static KEYWORD_TO_PACKS: LazyLock<HashMap<String, Vec<&'static str>>> = LazyLock::new(|| {
    let mut map: HashMap<String, Vec<&'static str>> = HashMap::new();
    for pack in ALL_PACKS {
        for kw in pack.keywords {
            map.entry(kw.to_ascii_lowercase())
                .or_default()
                .push(pack.id);
        }
    }
    map
});

// ============================================================================
// Per-pane Guard Configuration
// ============================================================================

/// Per-pane guard configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PaneGuardConfig {
    /// Trust level for this pane.
    pub trust_level: TrustLevel,
    /// Override enabled packs (None = use global policy).
    pub enabled_packs: Option<Vec<String>>,
    /// Per-pane allowlist — commands matching these patterns are always allowed.
    pub allowlist_patterns: Vec<String>,
    /// Max evaluation time in microseconds before fail-open.
    pub budget_us: u64,
}

impl Default for PaneGuardConfig {
    fn default() -> Self {
        Self {
            trust_level: TrustLevel::Strict,
            enabled_packs: None,
            allowlist_patterns: Vec::new(),
            budget_us: 100,
        }
    }
}

// ============================================================================
// Guard Policy
// ============================================================================

/// Centralized guard policy for the FrankenTerm instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GuardPolicy {
    /// Default trust level for new panes.
    pub default_trust: TrustLevel,
    /// Globally enabled security pack IDs.
    pub enabled_packs: Vec<String>,
    /// Globally disabled security pack IDs.
    pub disabled_packs: Vec<String>,
    /// Per-pane configuration overrides.
    pub pane_overrides: HashMap<u64, PaneGuardConfig>,
    /// Max audit log entries (ring buffer capacity).
    pub audit_capacity: usize,
}

impl Default for GuardPolicy {
    fn default() -> Self {
        Self {
            default_trust: TrustLevel::Strict,
            enabled_packs: ALL_PACKS.iter().map(|p| p.id.to_string()).collect(),
            disabled_packs: Vec::new(),
            pane_overrides: HashMap::new(),
            audit_capacity: 10_000,
        }
    }
}

// ============================================================================
// Audit Log
// ============================================================================

/// Audit entry for a guard evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Pane that the command was evaluated for.
    pub pane_id: u64,
    /// The command text (truncated to 256 chars).
    pub command: String,
    /// Decision outcome.
    pub decision: String,
    /// Matched rule ID (if any).
    pub rule_id: Option<String>,
    /// Matched pack (if any).
    pub pack: Option<String>,
    /// Evaluation duration in microseconds.
    pub eval_us: u64,
    /// Epoch seconds timestamp.
    pub timestamp_s: u64,
}

// ============================================================================
// Command Guard Engine
// ============================================================================

/// Native command guard engine — evaluates commands against security packs
/// with per-pane trust levels.
pub struct CommandGuard {
    policy: GuardPolicy,
    /// Compiled per-pane allowlist regexes (lazily populated).
    pane_allowlists: HashMap<u64, Vec<Regex>>,
    /// Ring buffer of audit entries.
    audit_log: Vec<AuditEntry>,
    audit_write_idx: usize,
}

impl CommandGuard {
    /// Create a new guard with the given policy.
    #[must_use]
    pub fn new(policy: GuardPolicy) -> Self {
        let capacity = policy.audit_capacity;
        Self {
            policy,
            pane_allowlists: HashMap::new(),
            audit_log: Vec::with_capacity(capacity.min(1024)),
            audit_write_idx: 0,
        }
    }

    /// Create a guard with default policy.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(GuardPolicy::default())
    }

    /// Evaluate a command for a specific pane.
    pub fn evaluate(&mut self, command: &str, pane_id: u64) -> GuardDecision {
        let start = Instant::now();
        let pane_config = self.pane_config(pane_id);
        let trust = pane_config.trust_level;

        // ReadOnly panes skip evaluation entirely.
        if trust == TrustLevel::ReadOnly {
            self.record(pane_id, command, "allow", None, None, 0);
            return GuardDecision::Allow;
        }

        // Check per-pane allowlist first.
        if self.matches_pane_allowlist(command, pane_id) {
            let elapsed = start.elapsed().as_micros() as u64;
            self.record(pane_id, command, "allow", None, None, elapsed);
            return GuardDecision::Allow;
        }

        // Quick-reject: check if command contains any pack keywords.
        let relevant_packs = self.keyword_filter(command);
        if relevant_packs.is_empty() {
            let elapsed = start.elapsed().as_micros() as u64;
            self.record(pane_id, command, "allow", None, None, elapsed);
            return GuardDecision::Allow;
        }

        // Scan relevant packs for destructive patterns.
        let decision = Self::scan_packs(command, &relevant_packs, trust);
        let elapsed = start.elapsed().as_micros() as u64;

        let (dec_str, rule, pack) = match &decision {
            GuardDecision::Allow => ("allow", None, None),
            GuardDecision::Block { rule_id, pack, .. } => {
                ("block", Some(rule_id.clone()), Some(pack.clone()))
            }
            GuardDecision::Warn { rule_id, pack, .. } => {
                ("warn", Some(rule_id.clone()), Some(pack.clone()))
            }
        };
        self.record(pane_id, command, dec_str, rule, pack, elapsed);

        decision
    }

    /// Evaluate a command and return the decision along with evaluation time in microseconds.
    pub fn evaluate_timed(&mut self, command: &str, pane_id: u64) -> (GuardDecision, u64) {
        let start = Instant::now();
        let decision = self.evaluate(command, pane_id);
        let elapsed = start.elapsed().as_micros() as u64;
        (decision, elapsed)
    }

    /// Pre-flight query: would this command be blocked for the given pane?
    /// Does not record to audit log.
    #[must_use]
    pub fn preflight(&self, command: &str, pane_id: u64) -> GuardDecision {
        let pane_config = self.pane_config(pane_id);
        let trust = pane_config.trust_level;

        if trust == TrustLevel::ReadOnly {
            return GuardDecision::Allow;
        }

        let relevant_packs = self.keyword_filter(command);
        if relevant_packs.is_empty() {
            return GuardDecision::Allow;
        }

        Self::scan_packs(command, &relevant_packs, trust)
    }

    /// Get the current audit log entries.
    #[must_use]
    pub fn audit_log(&self) -> &[AuditEntry] {
        &self.audit_log
    }

    /// Get the number of audit entries recorded.
    #[must_use]
    pub fn audit_count(&self) -> usize {
        self.audit_log.len()
    }

    /// Clear the audit log.
    pub fn clear_audit_log(&mut self) {
        self.audit_log.clear();
        self.audit_write_idx = 0;
    }

    /// Get the policy.
    #[must_use]
    pub fn policy(&self) -> &GuardPolicy {
        &self.policy
    }

    /// Update the policy.
    pub fn set_policy(&mut self, policy: GuardPolicy) {
        self.policy = policy;
        self.pane_allowlists.clear(); // Invalidate compiled allowlists.
    }

    /// Set per-pane configuration.
    pub fn set_pane_config(&mut self, pane_id: u64, config: PaneGuardConfig) {
        self.pane_allowlists.remove(&pane_id); // Invalidate cached allowlist.
        self.policy.pane_overrides.insert(pane_id, config);
    }

    /// List available security pack IDs.
    #[must_use]
    pub fn available_packs() -> Vec<&'static str> {
        ALL_PACKS.iter().map(|p| p.id).collect()
    }

    // ========================================================================
    // Internal
    // ========================================================================

    fn pane_config(&self, pane_id: u64) -> PaneGuardConfig {
        if let Some(override_config) = self.policy.pane_overrides.get(&pane_id) {
            return override_config.clone();
        }
        PaneGuardConfig {
            trust_level: self.policy.default_trust,
            ..PaneGuardConfig::default()
        }
    }

    fn matches_pane_allowlist(&mut self, command: &str, pane_id: u64) -> bool {
        let patterns = if let Some(cached) = self.pane_allowlists.get(&pane_id) {
            cached
        } else {
            let pane_config = self.pane_config(pane_id);
            let compiled: Vec<Regex> = pane_config
                .allowlist_patterns
                .iter()
                .filter_map(|p| Regex::new(p).ok())
                .collect();
            self.pane_allowlists.insert(pane_id, compiled);
            self.pane_allowlists.get(&pane_id).unwrap()
        };

        patterns.iter().any(|r| r.is_match(command))
    }

    fn keyword_filter(&self, command: &str) -> Vec<&'static str> {
        let mut pack_ids: Vec<&'static str> = Vec::new();
        let enabled = &self.policy.enabled_packs;
        let disabled = &self.policy.disabled_packs;

        for mat in KEYWORD_AUTOMATON.find_iter(command) {
            let matched = &command[mat.start()..mat.end()];
            let key = matched.to_ascii_lowercase();
            if let Some(packs) = KEYWORD_TO_PACKS.get(&key) {
                for pack_id in packs {
                    if enabled.iter().any(|e| e == pack_id)
                        && !disabled.iter().any(|d| d == pack_id)
                        && !pack_ids.contains(pack_id)
                    {
                        pack_ids.push(pack_id);
                    }
                }
            }
        }

        pack_ids
    }

    fn scan_packs(command: &str, relevant_pack_ids: &[&str], trust: TrustLevel) -> GuardDecision {
        for pack in ALL_PACKS {
            if !relevant_pack_ids.contains(&pack.id) {
                continue;
            }

            // Check safe patterns first (whitelist).
            let is_safe = pack
                .safe_rules
                .iter()
                .any(|safe| safe.pattern.is_match(command));
            if is_safe {
                continue;
            }

            // Check destructive patterns.
            for rule in pack.destructive_rules {
                if rule.pattern.is_match(command) {
                    let rule_id = rule.id.to_string();
                    let pack_name = pack.id.to_string();
                    let reason = rule.reason.to_string();
                    let suggestions: Vec<String> =
                        rule.suggestions.iter().map(|s| (*s).to_string()).collect();

                    return match trust {
                        TrustLevel::Strict => GuardDecision::Block {
                            rule_id,
                            pack: pack_name,
                            reason,
                            suggestions,
                        },
                        TrustLevel::Permissive => GuardDecision::Warn {
                            rule_id,
                            pack: pack_name,
                            reason,
                        },
                        TrustLevel::ReadOnly => GuardDecision::Allow,
                    };
                }
            }
        }

        GuardDecision::Allow
    }

    fn record(
        &mut self,
        pane_id: u64,
        command: &str,
        decision: &str,
        rule_id: Option<String>,
        pack: Option<String>,
        eval_us: u64,
    ) {
        let entry = AuditEntry {
            pane_id,
            command: if command.len() > 256 {
                format!("{}...", &command[..253])
            } else {
                command.to_string()
            },
            decision: decision.to_string(),
            rule_id,
            pack,
            eval_us,
            timestamp_s: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };

        if self.audit_log.len() < self.policy.audit_capacity {
            self.audit_log.push(entry);
        } else {
            let idx = self.audit_write_idx % self.policy.audit_capacity;
            self.audit_log[idx] = entry;
        }
        self.audit_write_idx += 1;
    }
}

/// Evaluate a command without guard state (stateless, for integration with policy.rs).
///
/// Returns `(decision, pack, reason, suggestions)` or `None` for allow.
#[must_use]
pub fn evaluate_stateless(command: &str) -> Option<(String, String, String, Vec<String>)> {
    // Quick-reject via keyword scan.
    let mut relevant_pack_ids: Vec<&str> = Vec::new();
    for mat in KEYWORD_AUTOMATON.find_iter(command) {
        let matched = &command[mat.start()..mat.end()];
        let key = matched.to_ascii_lowercase();
        if let Some(packs) = KEYWORD_TO_PACKS.get(&key) {
            for pack_id in packs {
                if !relevant_pack_ids.contains(pack_id) {
                    relevant_pack_ids.push(pack_id);
                }
            }
        }
    }

    if relevant_pack_ids.is_empty() {
        return None;
    }

    for pack in ALL_PACKS {
        if !relevant_pack_ids.contains(&pack.id) {
            continue;
        }

        // Safe patterns first.
        if pack
            .safe_rules
            .iter()
            .any(|safe| safe.pattern.is_match(command))
        {
            continue;
        }

        // Destructive patterns.
        for rule in pack.destructive_rules {
            if rule.pattern.is_match(command) {
                return Some((
                    rule.id.to_string(),
                    pack.id.to_string(),
                    rule.reason.to_string(),
                    rule.suggestions.iter().map(|s| (*s).to_string()).collect(),
                ));
            }
        }
    }

    None
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn strict_guard() -> CommandGuard {
        CommandGuard::new(GuardPolicy {
            default_trust: TrustLevel::Strict,
            ..GuardPolicy::default()
        })
    }

    fn permissive_guard() -> CommandGuard {
        CommandGuard::new(GuardPolicy {
            default_trust: TrustLevel::Permissive,
            ..GuardPolicy::default()
        })
    }

    fn readonly_guard() -> CommandGuard {
        CommandGuard::new(GuardPolicy {
            default_trust: TrustLevel::ReadOnly,
            ..GuardPolicy::default()
        })
    }

    // ========================================================================
    // Trust level behavior
    // ========================================================================

    #[test]
    fn strict_blocks_rm_rf() {
        let mut guard = strict_guard();
        let decision = guard.evaluate("rm -rf /tmp/important", 1);
        assert!(decision.is_blocked());
        assert_eq!(decision.rule_id(), Some("core.filesystem:rm-rf"));
    }

    #[test]
    fn permissive_warns_rm_rf() {
        let mut guard = permissive_guard();
        let decision = guard.evaluate("rm -rf /tmp/important", 1);
        assert!(decision.is_warning());
        assert_eq!(decision.rule_id(), Some("core.filesystem:rm-rf"));
    }

    #[test]
    fn readonly_allows_everything() {
        let mut guard = readonly_guard();
        let decision = guard.evaluate("rm -rf /", 1);
        assert!(decision.is_allowed());
    }

    // ========================================================================
    // Quick-reject: safe commands pass through
    // ========================================================================

    #[test]
    fn allows_safe_commands() {
        let mut guard = strict_guard();
        assert!(guard.evaluate("ls -la", 1).is_allowed());
        assert!(guard.evaluate("cat file.txt", 1).is_allowed());
        assert!(guard.evaluate("echo hello", 1).is_allowed());
        assert!(guard.evaluate("pwd", 1).is_allowed());
        assert!(guard.evaluate("cargo build", 1).is_allowed());
        assert!(guard.evaluate("rustc --version", 1).is_allowed());
    }

    #[test]
    fn allows_git_safe_commands() {
        let mut guard = strict_guard();
        assert!(guard.evaluate("git status", 1).is_allowed());
        assert!(guard.evaluate("git log --oneline", 1).is_allowed());
        assert!(guard.evaluate("git diff HEAD", 1).is_allowed());
        assert!(guard.evaluate("git add .", 1).is_allowed());
        assert!(guard.evaluate("git commit -m 'test'", 1).is_allowed());
        assert!(guard.evaluate("git pull origin main", 1).is_allowed());
        assert!(guard.evaluate("git push origin main", 1).is_allowed());
    }

    // ========================================================================
    // Filesystem pack
    // ========================================================================

    #[test]
    fn blocks_rm_rf_root() {
        let mut guard = strict_guard();
        let d = guard.evaluate("rm -rf /", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("core.filesystem:rm-rf-root"));
    }

    #[test]
    fn blocks_rm_rf_home() {
        let mut guard = strict_guard();
        let d = guard.evaluate("rm -rf ~", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("core.filesystem:rm-rf-root"));
    }

    #[test]
    fn allows_rm_rf_node_modules() {
        let mut guard = strict_guard();
        // Safe pattern whitelist: rm -rf node_modules
        let d = guard.evaluate("rm -rf node_modules", 1);
        assert!(d.is_allowed());
    }

    #[test]
    fn allows_rm_rf_target() {
        let mut guard = strict_guard();
        let d = guard.evaluate("rm -rf target", 1);
        assert!(d.is_allowed());
    }

    #[test]
    fn blocks_chmod_777() {
        let mut guard = strict_guard();
        let d = guard.evaluate("chmod -R 777 /var/www", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("core.filesystem:chmod-recursive-wide"));
    }

    #[test]
    fn blocks_dd_device() {
        let mut guard = strict_guard();
        let d = guard.evaluate("dd if=/dev/zero of=/dev/sda bs=1M", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("core.filesystem:dd-device"));
    }

    #[test]
    fn blocks_mkfs() {
        let mut guard = strict_guard();
        let d = guard.evaluate("mkfs.ext4 /dev/sdb1", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("core.filesystem:mkfs"));
    }

    // ========================================================================
    // Git pack
    // ========================================================================

    #[test]
    fn blocks_git_push_force() {
        let mut guard = strict_guard();
        let d = guard.evaluate("git push --force origin main", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("core.git:push-force"));
    }

    #[test]
    fn allows_git_push_force_with_lease() {
        let mut guard = strict_guard();
        // Safe pattern: --force-with-lease
        let d = guard.evaluate("git push --force-with-lease origin main", 1);
        assert!(d.is_allowed());
    }

    #[test]
    fn blocks_git_reset_hard() {
        let mut guard = strict_guard();
        let d = guard.evaluate("git reset --hard HEAD~1", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("core.git:reset-hard"));
    }

    #[test]
    fn blocks_git_clean_fd() {
        let mut guard = strict_guard();
        let d = guard.evaluate("git clean -fd", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("core.git:clean-fd"));
    }

    #[test]
    fn blocks_git_branch_d() {
        let mut guard = strict_guard();
        let d = guard.evaluate("git branch -D feature-old", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("core.git:branch-delete-force"));
    }

    #[test]
    fn blocks_git_stash_drop() {
        let mut guard = strict_guard();
        let d = guard.evaluate("git stash drop stash@{0}", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("core.git:stash-drop"));
    }

    #[test]
    fn blocks_git_stash_clear() {
        let mut guard = strict_guard();
        let d = guard.evaluate("git stash clear", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("core.git:stash-drop"));
    }

    // ========================================================================
    // Database pack
    // ========================================================================

    #[test]
    fn blocks_drop_table() {
        let mut guard = strict_guard();
        let d = guard.evaluate("DROP TABLE users", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("database:drop"));
    }

    #[test]
    fn blocks_drop_database() {
        let mut guard = strict_guard();
        let d = guard.evaluate("DROP DATABASE production", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("database:drop"));
    }

    #[test]
    fn blocks_truncate_table() {
        let mut guard = strict_guard();
        let d = guard.evaluate("TRUNCATE TABLE sessions", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("database:truncate"));
    }

    #[test]
    fn blocks_delete_without_where() {
        let mut guard = strict_guard();
        let d = guard.evaluate("DELETE FROM users;", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("database:delete-no-where"));
    }

    // ========================================================================
    // Container pack
    // ========================================================================

    #[test]
    fn blocks_docker_system_prune() {
        let mut guard = strict_guard();
        let d = guard.evaluate("docker system prune -af", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("containers:system-prune"));
    }

    #[test]
    fn blocks_docker_volume_rm() {
        let mut guard = strict_guard();
        let d = guard.evaluate("docker volume prune", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("containers:volume-rm"));
    }

    // ========================================================================
    // Kubernetes pack
    // ========================================================================

    #[test]
    fn blocks_kubectl_delete_namespace() {
        let mut guard = strict_guard();
        let d = guard.evaluate("kubectl delete namespace production", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("kubernetes:delete-namespace"));
    }

    #[test]
    fn blocks_kubectl_delete_all() {
        let mut guard = strict_guard();
        let d = guard.evaluate("kubectl delete pods --all", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("kubernetes:delete-all"));
    }

    #[test]
    fn blocks_helm_uninstall() {
        let mut guard = strict_guard();
        let d = guard.evaluate("helm uninstall my-release", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("kubernetes:helm-uninstall"));
    }

    // ========================================================================
    // Cloud pack
    // ========================================================================

    #[test]
    fn blocks_terraform_destroy() {
        let mut guard = strict_guard();
        let d = guard.evaluate("terraform destroy -auto-approve", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("cloud:terraform-destroy"));
    }

    #[test]
    fn blocks_aws_s3_rm_recursive() {
        let mut guard = strict_guard();
        let d = guard.evaluate("aws s3 rm s3://my-bucket --recursive", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("cloud:aws-s3-rm-recursive"));
    }

    // ========================================================================
    // System pack
    // ========================================================================

    #[test]
    fn blocks_kill_9() {
        let mut guard = strict_guard();
        let d = guard.evaluate("kill -9 1234", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("system:kill-9"));
    }

    #[test]
    fn blocks_reboot() {
        let mut guard = strict_guard();
        let d = guard.evaluate("sudo reboot", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("system:reboot"));
    }

    // ========================================================================
    // Package manager pack
    // ========================================================================

    #[test]
    fn blocks_npm_unpublish() {
        let mut guard = strict_guard();
        let d = guard.evaluate("npm unpublish my-package@1.0.0", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("package_managers:npm-unpublish"));
    }

    #[test]
    fn blocks_cargo_yank() {
        let mut guard = strict_guard();
        let d = guard.evaluate("cargo yank --version 1.0.0 my-crate", 1);
        assert!(d.is_blocked());
        assert_eq!(d.rule_id(), Some("package_managers:cargo-yank"));
    }

    // ========================================================================
    // Per-pane override
    // ========================================================================

    #[test]
    fn per_pane_trust_override() {
        let mut guard = strict_guard();
        guard.set_pane_config(
            42,
            PaneGuardConfig {
                trust_level: TrustLevel::Permissive,
                ..PaneGuardConfig::default()
            },
        );
        // Pane 42 should warn (permissive), pane 1 should block (strict default).
        let d42 = guard.evaluate("rm -rf /tmp/data", 42);
        let d1 = guard.evaluate("rm -rf /tmp/data", 1);
        assert!(d42.is_warning());
        assert!(d1.is_blocked());
    }

    #[test]
    fn per_pane_allowlist() {
        let mut guard = strict_guard();
        guard.set_pane_config(
            10,
            PaneGuardConfig {
                trust_level: TrustLevel::Strict,
                allowlist_patterns: vec![r"^rm -rf /tmp/test".to_string()],
                ..PaneGuardConfig::default()
            },
        );
        let d = guard.evaluate("rm -rf /tmp/test", 10);
        assert!(d.is_allowed());
    }

    // ========================================================================
    // Disabled packs
    // ========================================================================

    #[test]
    fn disabled_pack_skipped() {
        let mut guard = CommandGuard::new(GuardPolicy {
            default_trust: TrustLevel::Strict,
            disabled_packs: vec!["database".to_string()],
            ..GuardPolicy::default()
        });
        // Database pack disabled, so DROP TABLE should be allowed.
        let d = guard.evaluate("DROP TABLE users", 1);
        assert!(d.is_allowed());
    }

    // ========================================================================
    // Audit log
    // ========================================================================

    #[test]
    fn audit_log_records_entries() {
        let mut guard = strict_guard();
        guard.evaluate("ls -la", 1);
        guard.evaluate("rm -rf /tmp/data", 2);
        assert_eq!(guard.audit_count(), 2);

        let log = guard.audit_log();
        assert_eq!(log[0].decision, "allow");
        assert_eq!(log[0].pane_id, 1);
        assert_eq!(log[1].decision, "block");
        assert_eq!(log[1].pane_id, 2);
    }

    #[test]
    fn audit_log_ring_buffer_wraps() {
        let mut guard = CommandGuard::new(GuardPolicy {
            audit_capacity: 3,
            ..GuardPolicy::default()
        });
        for i in 0..5 {
            guard.evaluate("ls", i);
        }
        // Ring buffer should have exactly 3 entries after wrapping.
        assert_eq!(guard.audit_count(), 3);
    }

    // ========================================================================
    // Stateless evaluation
    // ========================================================================

    #[test]
    fn stateless_detects_destructive() {
        let result = evaluate_stateless("git reset --hard HEAD");
        assert!(result.is_some());
        let (rule_id, pack, _, _) = result.unwrap();
        assert_eq!(rule_id, "core.git:reset-hard");
        assert_eq!(pack, "core.git");
    }

    #[test]
    fn stateless_allows_safe() {
        assert!(evaluate_stateless("git status").is_none());
        assert!(evaluate_stateless("ls -la").is_none());
        assert!(evaluate_stateless("echo hello").is_none());
    }

    // ========================================================================
    // Preflight query (no audit)
    // ========================================================================

    #[test]
    fn preflight_does_not_record_audit() {
        let guard = strict_guard();
        let d = guard.preflight("rm -rf /tmp/data", 1);
        assert!(d.is_blocked());
        assert_eq!(guard.audit_count(), 0);
    }

    // ========================================================================
    // Available packs
    // ========================================================================

    #[test]
    fn lists_available_packs() {
        let packs = CommandGuard::available_packs();
        assert!(packs.contains(&"core.filesystem"));
        assert!(packs.contains(&"core.git"));
        assert!(packs.contains(&"database"));
        assert!(packs.contains(&"containers"));
        assert!(packs.contains(&"kubernetes"));
        assert!(packs.contains(&"cloud"));
        assert!(packs.contains(&"system"));
        assert!(packs.contains(&"package_managers"));
        assert_eq!(packs.len(), 8);
    }

    // ========================================================================
    // Decision serialization
    // ========================================================================

    #[test]
    fn guard_decision_serializes() {
        let d = GuardDecision::Block {
            rule_id: "core.git:push-force".to_string(),
            pack: "core.git".to_string(),
            reason: "Force push rewrites remote history".to_string(),
            suggestions: vec!["Use --force-with-lease".to_string()],
        };
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains("\"decision\":\"block\""));
        assert!(json.contains("core.git:push-force"));
    }

    // ========================================================================
    // Edge cases
    // ========================================================================

    #[test]
    fn empty_command_allowed() {
        let mut guard = strict_guard();
        assert!(guard.evaluate("", 1).is_allowed());
    }

    #[test]
    fn whitespace_command_allowed() {
        let mut guard = strict_guard();
        assert!(guard.evaluate("   ", 1).is_allowed());
    }

    #[test]
    fn very_long_command_evaluated() {
        let mut guard = strict_guard();
        let long_cmd = format!("echo {} && rm -rf /tmp/data", "x".repeat(10_000));
        let d = guard.evaluate(&long_cmd, 1);
        assert!(d.is_blocked());
    }

    #[test]
    fn audit_truncates_long_commands() {
        let mut guard = strict_guard();
        let long_cmd = "ls ".to_string() + &"x".repeat(500);
        guard.evaluate(&long_cmd, 1);
        let log = guard.audit_log();
        assert!(log[0].command.len() <= 260);
        assert!(log[0].command.ends_with("..."));
    }
}
