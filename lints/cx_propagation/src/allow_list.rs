//! Allow-list for the cx-propagation lint.
//!
//! Two kinds of carve-outs, both load-bearing.
//!
//! 1. [`EXEMPT_FILES`] — runtime-layer modules that *are* the seal.
//!    They define `Cx` and `RuntimeProof` themselves; requiring them
//!    to take a `Cx` parameter is circular.
//!
//! 2. [`WRAPPER_EXEMPTIONS`] — ergonomic wrappers around a `_with_cx`
//!    sibling. The wrapper constructs a default `Cx` internally and
//!    delegates. Each entry must be paired with a real covered
//!    sibling on the same file (the analyzer enforces this — a stale
//!    wrapper exemption is a lint failure).
//!
//! The two lists are kept in sync with the Python audit script at
//! `scripts/check_runtime_proof_coverage.py`. They share a single
//! source of truth: `docs/runtime/cx-propagation-lint.md` documents
//! the decision tree for adding entries.
//!
//! # Adding a new exemption
//!
//! 1. Confirm the function is genuinely a wrapper (constructs a
//!    default `Cx`, delegates to a covered sibling) — *not* an
//!    "I haven't gotten around to threading Cx yet" placeholder.
//! 2. Add a comment in the source explaining why the wrapper is
//!    safe to exempt.
//! 3. Add an entry here AND in `scripts/check_runtime_proof_coverage.py`.
//!    The two lists must stay in lockstep; CI guards against drift.

/// Runtime-layer modules that *are* the seal. They cannot take a
/// `Cx` parameter on themselves without circular dependency. Add
/// sparingly — every entry is permanent doctrine.
pub const EXEMPT_FILES: &[&str] = &[
    "runtime_async.rs", // wrapper module — primitives sealed elsewhere
    "runtime_proof.rs", // defines the seal itself
    "cx.rs",            // the canonical structured-async witness
    "cx_stub.rs",       // build-time stub of cx.rs (no-op shim)
];

/// Ergonomic wrappers around a `_with_cx` / `_cx` sibling.
///
/// Each entry is a `(relative_path, fn_name)` pair. Paths are
/// rooted at `crates/frankenterm-core/src/` and use forward
/// slashes (matches the Python audit script).
///
/// **Substrate-shippable subset**: the audit script's
/// `WRAPPER_EXEMPTIONS` is currently 150+ entries deep. The
/// substrate ship below carries the head of the list — the
/// load-bearing wrappers in the runtime/cx machinery. Filling
/// out the long tail is filed as ft-t9a6q.1.cont.allowlist; a
/// sweep that compares this list against the Python list and
/// surfaces drift is filed as ft-t9a6q.1.cont.drift.
///
/// CI today runs the *audit script* (Python) against the full
/// allow-list; this analyzer runs against a fixture corpus to
/// verify the rule shape. Once the long tail is ported here, the
/// analyzer can replace the audit script as the source of truth.
pub const WRAPPER_EXEMPTIONS: &[(&str, &str)] = &[
    // Runtime layer — wrappers that construct a default Cx and
    // delegate. Each has a covered `_with_cx` sibling.
    ("runtime.rs", "spawn"),
    ("runtime.rs", "spawn_blocking"),
    ("runtime.rs", "yield_now"),
    ("runtime.rs", "sleep"),
    // Cancellation primitives — the no-Cx form is the public
    // ergonomic surface; the _with_cx form is the canonical
    // sealed entry.
    ("cancellation.rs", "cancel"),
    ("cancellation.rs", "is_cancelled"),
    // Wait helpers — same pattern.
    ("wait.rs", "until"),
    ("wait.rs", "until_with_timeout"),
];
