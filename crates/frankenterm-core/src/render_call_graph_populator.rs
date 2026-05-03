//! Render call-graph populator (br-ft-ept8r sub-task 2).
//!
//! Text-based grep harness that populates the substrate's
//! [`CallGraph`] from frankenterm-gui crate source so the
//! integration's CI release gate can call
//! [`audit_render_call_graph`] without a custom dylint
//! plugin.
//!
//! [`CallGraph`]: crate::render_call_graph_audit::CallGraph
//! [`audit_render_call_graph`]: crate::render_call_graph_audit::audit_render_call_graph
//!
//! ## Design
//!
//! Heuristic regex pass — NOT a full Rust syntax tree. The
//! audit runs at CI time, so false negatives (dynamic
//! dispatch, macro-generated calls) are acceptable; false
//! positives (a "triple_buffer.read()" inside a string
//! literal) would noisily fail CI, so the pass strips
//! line-comments and `// ` body content. Block comments and
//! multi-line strings are out of scope (the populator's
//! audit corpus is hand-written test code that doesn't use
//! these forms in render hot paths).
//!
//! The populator walks each source file:
//!
//! 1. Match `fn <name>` definitions; assign each a
//!    [`CallSiteId`] keyed by name. Record line for
//!    diagnostics.
//! 2. Track brace depth from the opening `{` of each
//!    function. Calls inside the body create edges from the
//!    enclosing function's id.
//! 3. Match guard-construction patterns (configurable):
//!    `triple_buffer.read()` → `SnapshotKind::ReadOnly`,
//!    `triple_buffer.write()` / `triple_buffer.write_guard()`
//!    → `SnapshotKind::Mutation`. Each match assigns a fresh
//!    [`CallSiteId`] for the guard site itself; add an edge
//!    from the enclosing function.
//! 4. Match render-entry function names from a configurable
//!    allowlist (default: `paint_impl`, `render_pane`,
//!    `render_line`, `screen_line_render`). Emit
//!    [`RenderEntryPoint`] for each.
//!
//! After every file is processed, a second pass resolves
//! function-call edges: for each `name(` token inside a
//! function body, look up `name` in the function-to-id map
//! and add an edge if present. Names not in the map (extern
//! crates, methods on types, etc.) are dropped — they can't
//! transitively reach a substrate guard anyway.
//!
//! ## What this module is NOT
//!
//! - Not a full Rust parser. `syn` would catch every edge but
//!   pulls a heavy dep into core; the bead explicitly allows
//!   "custom dylint plugin or grep+regex harness" so this
//!   ships the lighter option.
//! - Not a clippy lint. The audit runs as a CI check via the
//!   driver in sub-task 3; it doesn't need clippy's plumbing.

#![allow(clippy::needless_range_loop)]

use std::collections::HashMap;

use regex::Regex;
use std::sync::OnceLock;

use crate::render_call_graph_audit::{
    CallEdge, CallGraph, CallSiteId, GuardConstructionSite, RenderEntryPoint,
};
use crate::render_snapshot_guard::SnapshotKind;

// ============================================================================
// Config
// ============================================================================

/// Regex patterns + entry-point allowlist that drive the
/// populator. Defaults match the bead's documented call sites.
#[derive(Debug, Clone)]
pub struct PopulatorConfig {
    /// Function names treated as render entry points. Each
    /// match produces a [`RenderEntryPoint`].
    pub render_entry_names: Vec<String>,
    /// Pattern that captures `triple_buffer.read()`-shaped
    /// guard sites. Default uses a lookahead for `.read(`.
    pub readonly_guard_pattern: String,
    /// Pattern that captures `triple_buffer.write()` /
    /// `.write_guard()`-shaped sites.
    pub mutation_guard_pattern: String,
}

impl Default for PopulatorConfig {
    fn default() -> Self {
        Self {
            render_entry_names: vec![
                "paint_impl".to_string(),
                "render_pane".to_string(),
                "render_line".to_string(),
                "screen_line_render".to_string(),
            ],
            readonly_guard_pattern: r"triple_buffer\s*\.\s*read\s*\(".to_string(),
            mutation_guard_pattern: r"triple_buffer\s*\.\s*(?:write_guard|write)\s*\(".to_string(),
        }
    }
}

// ============================================================================
// Output
// ============================================================================

/// Populator output. The integration's CI driver feeds these
/// fields into [`audit_render_call_graph`].
///
/// [`audit_render_call_graph`]: crate::render_call_graph_audit::audit_render_call_graph
#[derive(Debug, Clone, Default)]
pub struct PopulatorOutput {
    pub graph: CallGraph,
    pub guard_sites: Vec<GuardConstructionSite>,
    pub entry_points: Vec<RenderEntryPoint>,
    /// Function name → assigned [`CallSiteId`]. Useful to the
    /// CI driver for emitting diagnostics that reference the
    /// human-readable name.
    pub function_to_id: HashMap<String, CallSiteId>,
}

// ============================================================================
// Heuristic regexes (compiled once)
// ============================================================================

fn fn_def_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        // pub / pub(...) / async / unsafe / const / extern modifiers
        // followed by `fn name`
        Regex::new(
            r"^\s*(?:pub(?:\s*\([^)]*\))?\s+)?(?:(?:async|unsafe|const|extern(?:\s*\x22[^\x22]*\x22)?)\s+)*fn\s+([a-zA-Z_][a-zA-Z0-9_]*)",
        )
        .expect("fn_def_regex compiles")
    })
}

fn call_token_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"\b([a-zA-Z_][a-zA-Z0-9_]*)\s*\(").expect("call_token_regex compiles")
    })
}

// ============================================================================
// Comment / string stripping
// ============================================================================

/// Strip everything after the first un-escaped `//` on a
/// line, treating `"`-delimited strings as opaque so a
/// comment inside a string literal isn't dropped.
///
/// Block comments and raw strings (`r"..."`) are out of
/// scope; the audit corpus is hand-written test code and
/// upstream paint.rs which don't use these in hot-path lines.
#[must_use]
pub fn strip_line_comment(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    let mut in_str = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            out.push(b as char);
            if b == b'\\' && i + 1 < bytes.len() {
                // Preserve escaped char as-is.
                out.push(bytes[i + 1] as char);
                i += 2;
                continue;
            }
            if b == b'"' {
                in_str = false;
            }
            i += 1;
        } else if b == b'"' {
            in_str = true;
            out.push('"');
            i += 1;
        } else if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            // Line comment — drop the rest of the line.
            break;
        } else {
            out.push(b as char);
            i += 1;
        }
    }
    out
}

/// Replace string-literal contents with spaces so call-token
/// regexes don't match inside `"foo()"`. Comments must be
/// stripped first.
#[must_use]
pub fn blank_string_literals(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    let mut in_str = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            if b == b'\\' && i + 1 < bytes.len() {
                out.push(' ');
                out.push(' ');
                i += 2;
                continue;
            }
            if b == b'"' {
                in_str = false;
                out.push('"');
                i += 1;
                continue;
            }
            out.push(' ');
            i += 1;
        } else if b == b'"' {
            in_str = true;
            out.push('"');
            i += 1;
        } else {
            out.push(b as char);
            i += 1;
        }
    }
    out
}

// ============================================================================
// Brace-depth tracker
// ============================================================================

/// Count net `{` minus `}` in a line, ignoring those inside
/// quoted strings (uses [`blank_string_literals`] before
/// counting).
#[must_use]
pub fn line_brace_delta(stripped: &str) -> i32 {
    let blanked = blank_string_literals(stripped);
    let opens = blanked.bytes().filter(|&b| b == b'{').count() as i32;
    let closes = blanked.bytes().filter(|&b| b == b'}').count() as i32;
    opens - closes
}

// ============================================================================
// Per-file walker
// ============================================================================

/// Walk one source file and update `output` in place.
///
/// `next_id` is the populator's monotonic id counter; the
/// driver passes the same `&mut u32` across files so every
/// guard site / function gets a globally unique id.
pub fn populate_from_source(
    file_path: &str,
    source: &str,
    config: &PopulatorConfig,
    next_id: &mut u32,
    output: &mut PopulatorOutput,
) {
    let readonly_re =
        Regex::new(&config.readonly_guard_pattern).expect("readonly_guard_pattern compiles");
    let mutation_re =
        Regex::new(&config.mutation_guard_pattern).expect("mutation_guard_pattern compiles");

    // Collect (line, fn_name, fn_start_brace_depth) so the
    // call-edge pass knows which function each line belongs
    // to. The walker tracks current brace depth and the stack
    // of enclosing functions: a function's body is the lines
    // from its opening `{` to the matching `}` at the same
    // depth.
    let mut depth: i32 = 0;
    // Stack of (fn_name, fn_id, fn_open_depth). Closures push
    // an empty string to occupy the slot without affecting
    // edge attribution.
    let mut stack: Vec<(String, CallSiteId, i32)> = Vec::new();

    for (line_idx, raw_line) in source.lines().enumerate() {
        let stripped = strip_line_comment(raw_line);
        let blanked = blank_string_literals(&stripped);

        // 1) Function definitions on this line.
        if let Some(caps) = fn_def_regex().captures(&blanked) {
            let name = caps
                .get(1)
                .expect("group 1 always matches")
                .as_str()
                .to_string();
            let id = CallSiteId(*next_id);
            *next_id += 1;
            output.function_to_id.insert(name.clone(), id);

            // Render entry point check.
            if config.render_entry_names.iter().any(|n| n == &name) {
                output.entry_points.push(RenderEntryPoint {
                    id,
                    function_name: name.clone(),
                });
            }

            // Push function onto stack ONCE the body opens.
            // The fn signature line might end with `{` or
            // continue across multiple lines (`-> Result<T,
            // E>`). The brace tracker handles either: when we
            // see a `{` on this same line, depth bumps and
            // the function is "open". If the signature spans
            // lines, the `{` will arrive on a later line
            // (depth bumps then), but no fn body work happens
            // before then.
            stack.push((name, id, depth));
        }

        // 2) Guard-construction sites on this line.
        let enclosing = stack.last().map(|(_, id, _)| *id);
        for m in readonly_re.find_iter(&blanked) {
            let guard_id = CallSiteId(*next_id);
            *next_id += 1;
            output.guard_sites.push(GuardConstructionSite {
                id: guard_id,
                snapshot_kind: SnapshotKind::ReadOnly,
                file: file_path.to_string(),
                line: (line_idx + 1) as u32,
            });
            if let Some(caller) = enclosing {
                output.graph.add_edge(caller, guard_id);
            }
            // `m` already used; suppress unused-binding lint
            // by referring to it.
            let _ = m;
        }
        for m in mutation_re.find_iter(&blanked) {
            let guard_id = CallSiteId(*next_id);
            *next_id += 1;
            output.guard_sites.push(GuardConstructionSite {
                id: guard_id,
                snapshot_kind: SnapshotKind::Mutation,
                file: file_path.to_string(),
                line: (line_idx + 1) as u32,
            });
            if let Some(caller) = enclosing {
                output.graph.add_edge(caller, guard_id);
            }
            let _ = m;
        }

        // 3) Brace tracking. After processing tokens we count
        // braces on the line and pop stack entries whose
        // depth-at-open is no longer enclosed.
        let delta = line_brace_delta(&stripped);
        depth += delta;
        // Pop functions whose body has now closed.
        while let Some((_, _, fn_open_depth)) = stack.last() {
            if depth <= *fn_open_depth {
                stack.pop();
            } else {
                break;
            }
        }
    }
}

// ============================================================================
// Cross-file call-edge resolver
// ============================================================================

/// Second pass: resolve function-call edges using the
/// `function_to_id` map. Walks every source file again, maps
/// `name(` tokens to known function ids, and adds edges from
/// the enclosing function.
pub fn resolve_call_edges_from_source(
    source: &str,
    function_to_id: &HashMap<String, CallSiteId>,
    graph: &mut CallGraph,
) {
    let mut depth: i32 = 0;
    let mut stack: Vec<(CallSiteId, i32)> = Vec::new();

    for raw_line in source.lines() {
        let stripped = strip_line_comment(raw_line);
        let blanked = blank_string_literals(&stripped);

        // Function definitions push to stack.
        if let Some(caps) = fn_def_regex().captures(&blanked) {
            let name = caps.get(1).expect("group 1").as_str();
            if let Some(id) = function_to_id.get(name) {
                stack.push((*id, depth));
            }
        }

        // Call tokens on this line.
        let enclosing = stack.last().map(|(id, _)| *id);
        if let Some(caller) = enclosing {
            for caps in call_token_regex().captures_iter(&blanked) {
                let callee_name = caps.get(1).expect("group 1").as_str();
                // Skip the function definition's own name (so
                // the fn line `fn foo(args)` doesn't add
                // foo→foo).
                if let Some(callee_id) = function_to_id.get(callee_name) {
                    if *callee_id == caller {
                        continue;
                    }
                    graph.add_edge(caller, *callee_id);
                }
            }
        }

        let delta = line_brace_delta(&stripped);
        depth += delta;
        while let Some((_, fn_open_depth)) = stack.last() {
            if depth <= *fn_open_depth {
                stack.pop();
            } else {
                break;
            }
        }
    }
}

/// Top-level driver: process a list of (file_path, source)
/// pairs and return a fully populated [`PopulatorOutput`].
///
/// Calls [`populate_from_source`] then
/// [`resolve_call_edges_from_source`] for each file using a
/// shared id counter and shared output.
#[must_use]
pub fn populate_from_sources(
    files: &[(String, String)],
    config: &PopulatorConfig,
) -> PopulatorOutput {
    let mut output = PopulatorOutput::default();
    let mut next_id: u32 = 1;

    for (path, source) in files {
        populate_from_source(path, source, config, &mut next_id, &mut output);
    }

    let function_to_id = output.function_to_id.clone();
    for (_, source) in files {
        resolve_call_edges_from_source(source, &function_to_id, &mut output.graph);
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_call_graph_audit::{AuditConfig, AuditOutcome, audit_render_call_graph};

    // br-ft-x2oyy: render-call-graph fixtures intentionally embed
    // `triple_buffer.write()` snippets as synthetic source text. The dropped
    // write Results are test corpus data for the mutation-guard parser, not
    // production swallows.

    // ----------------------------------------------------------------
    // strip_line_comment
    // ----------------------------------------------------------------

    #[test]
    fn strip_line_comment_drops_trailing_comment() {
        assert_eq!(strip_line_comment("let x = 1; // hello"), "let x = 1; ");
    }

    #[test]
    fn strip_line_comment_preserves_slash_inside_string() {
        assert_eq!(
            strip_line_comment(r#"let s = "a // b";"#),
            r#"let s = "a // b";"#
        );
    }

    #[test]
    fn strip_line_comment_preserves_escaped_quote_inside_string() {
        assert_eq!(
            strip_line_comment(r#"let s = "a\"b // c"; // tail"#),
            r#"let s = "a\"b // c"; "#
        );
    }

    // ----------------------------------------------------------------
    // blank_string_literals
    // ----------------------------------------------------------------

    #[test]
    fn blank_string_literals_replaces_contents_with_spaces() {
        // Quotes preserved; inner bytes blanked.
        let out = blank_string_literals(r#"call("hidden");"#);
        assert!(out.starts_with("call(\""));
        assert!(out.ends_with("\");"));
        // Inner six chars (h,i,d,d,e,n) all spaces.
        assert!(!out.contains("hidden"));
    }

    // ----------------------------------------------------------------
    // line_brace_delta
    // ----------------------------------------------------------------

    #[test]
    fn brace_delta_counts_net_balance() {
        assert_eq!(line_brace_delta("fn foo() {"), 1);
        assert_eq!(line_brace_delta("} else {"), 0);
        assert_eq!(line_brace_delta("}"), -1);
        assert_eq!(line_brace_delta("if x { y(); }"), 0);
    }

    #[test]
    fn brace_delta_ignores_braces_in_strings() {
        assert_eq!(line_brace_delta(r#"let s = "} { }";"#), 0);
    }

    // ----------------------------------------------------------------
    // Single-file populate: one render entry, one ReadOnly guard
    // ----------------------------------------------------------------

    #[test]
    fn populates_render_entry_with_readonly_guard() {
        let src = r#"
fn paint_impl() {
    let snap = triple_buffer.read();
    let _ = snap;
}
"#;
        let cfg = PopulatorConfig::default();
        let mut next_id = 1u32;
        let mut out = PopulatorOutput::default();
        populate_from_source("paint.rs", src, &cfg, &mut next_id, &mut out);

        assert_eq!(out.entry_points.len(), 1);
        assert_eq!(out.entry_points[0].function_name, "paint_impl");
        assert_eq!(out.guard_sites.len(), 1);
        assert_eq!(out.guard_sites[0].snapshot_kind, SnapshotKind::ReadOnly);
        // Edge from paint_impl → guard.
        let entry_id = out.entry_points[0].id;
        let guard_id = out.guard_sites[0].id;
        let reachable = out.graph.reachable_from(entry_id);
        assert!(reachable.contains(&guard_id));
    }

    // ----------------------------------------------------------------
    // Mutation guard at render entry → audit reports violation
    // ----------------------------------------------------------------

    #[test]
    fn mutation_guard_at_render_entry_fails_audit() {
        let src = r#"
fn paint_impl() {
    let snap = triple_buffer.write();
    let _ = snap;
}
"#;
        let out = populate_from_sources(
            &[("paint.rs".to_string(), src.to_string())],
            &PopulatorConfig::default(),
        );
        assert_eq!(out.guard_sites.len(), 1);
        assert_eq!(out.guard_sites[0].snapshot_kind, SnapshotKind::Mutation);

        let outcome = audit_render_call_graph(
            &out.graph,
            &out.entry_points,
            &out.guard_sites,
            AuditConfig::default(),
        );
        match outcome {
            AuditOutcome::FailedWithViolations { violations } => {
                assert_eq!(violations.len(), 1);
            }
            other => panic!("expected FailedWithViolations, got {other:?}"),
        }
    }

    // ----------------------------------------------------------------
    // Transitive reachability: render_entry → helper → guard
    // ----------------------------------------------------------------

    #[test]
    fn transitive_call_to_helper_with_mutation_guard_fails_audit() {
        let src = r#"
fn paint_impl() {
    helper_that_writes();
}

fn helper_that_writes() {
    let snap = triple_buffer.write();
    let _ = snap;
}
"#;
        let out = populate_from_sources(
            &[("paint.rs".to_string(), src.to_string())],
            &PopulatorConfig::default(),
        );
        // Two function defs.
        assert_eq!(out.function_to_id.len(), 2);
        // Edge from paint_impl → helper_that_writes via call-edge resolver.
        let paint_id = *out
            .function_to_id
            .get("paint_impl")
            .expect("paint_impl recorded");
        let helper_id = *out
            .function_to_id
            .get("helper_that_writes")
            .expect("helper recorded");
        let reachable = out.graph.reachable_from(paint_id);
        assert!(reachable.contains(&helper_id));

        let outcome = audit_render_call_graph(
            &out.graph,
            &out.entry_points,
            &out.guard_sites,
            AuditConfig::default(),
        );
        assert!(matches!(outcome, AuditOutcome::FailedWithViolations { .. }));
    }

    // ----------------------------------------------------------------
    // Render entry calls helper that uses ReadOnly guard → pass
    // ----------------------------------------------------------------

    #[test]
    fn transitive_readonly_guard_passes_audit() {
        let src = r#"
fn paint_impl() {
    helper_that_reads();
}

fn helper_that_reads() {
    let snap = triple_buffer.read();
    let _ = snap;
}
"#;
        let out = populate_from_sources(
            &[("paint.rs".to_string(), src.to_string())],
            &PopulatorConfig::default(),
        );
        let outcome = audit_render_call_graph(
            &out.graph,
            &out.entry_points,
            &out.guard_sites,
            AuditConfig::default(),
        );
        assert_eq!(outcome, AuditOutcome::Pass);
    }

    // ----------------------------------------------------------------
    // Mutation guard in a NON-render-reachable function → pass
    // ----------------------------------------------------------------

    #[test]
    fn mutation_guard_off_render_path_passes_audit() {
        // mutation_only_path has the mutation guard but is
        // never called from any render entry → audit passes.
        let src = r#"
fn paint_impl() {
    helper_that_reads();
}

fn helper_that_reads() {
    let snap = triple_buffer.read();
    let _ = snap;
}

fn mutation_only_path() {
    let snap = triple_buffer.write();
    let _ = snap;
}
"#;
        let out = populate_from_sources(
            &[("paint.rs".to_string(), src.to_string())],
            &PopulatorConfig::default(),
        );
        // Mutation guard exists but unreachable from render entry.
        assert!(
            out.guard_sites
                .iter()
                .any(|g| g.snapshot_kind == SnapshotKind::Mutation)
        );
        let outcome = audit_render_call_graph(
            &out.graph,
            &out.entry_points,
            &out.guard_sites,
            AuditConfig::default(),
        );
        assert_eq!(outcome, AuditOutcome::Pass);
    }

    // ----------------------------------------------------------------
    // Comment / string-literal robustness
    // ----------------------------------------------------------------

    #[test]
    fn comment_with_guard_pattern_is_ignored() {
        let src = r#"
fn paint_impl() {
    // triple_buffer.write() — used to be here, now removed
    let snap = triple_buffer.read();
    let _ = snap;
}
"#;
        let out = populate_from_sources(
            &[("paint.rs".to_string(), src.to_string())],
            &PopulatorConfig::default(),
        );
        assert_eq!(out.guard_sites.len(), 1);
        assert_eq!(out.guard_sites[0].snapshot_kind, SnapshotKind::ReadOnly);
    }

    #[test]
    fn string_literal_with_guard_pattern_is_ignored() {
        let src = r#"
fn paint_impl() {
    let s = "triple_buffer.write() // not real";
    let snap = triple_buffer.read();
    let _ = snap;
    let _ = s;
}
"#;
        let out = populate_from_sources(
            &[("paint.rs".to_string(), src.to_string())],
            &PopulatorConfig::default(),
        );
        assert_eq!(out.guard_sites.len(), 1);
        assert_eq!(out.guard_sites[0].snapshot_kind, SnapshotKind::ReadOnly);
    }

    // ----------------------------------------------------------------
    // Cross-file edges
    // ----------------------------------------------------------------

    #[test]
    fn cross_file_call_edge_resolves() {
        let paint_src = r#"
fn paint_impl() {
    helper_in_other_file();
}
"#;
        let helper_src = r#"
fn helper_in_other_file() {
    let snap = triple_buffer.write();
    let _ = snap;
}
"#;
        let out = populate_from_sources(
            &[
                ("paint.rs".to_string(), paint_src.to_string()),
                ("helper.rs".to_string(), helper_src.to_string()),
            ],
            &PopulatorConfig::default(),
        );
        let outcome = audit_render_call_graph(
            &out.graph,
            &out.entry_points,
            &out.guard_sites,
            AuditConfig::default(),
        );
        assert!(
            matches!(outcome, AuditOutcome::FailedWithViolations { .. }),
            "cross-file mutation guard reachable from paint_impl must fail audit"
        );
    }

    // ----------------------------------------------------------------
    // pub / async / unsafe / const modifiers
    // ----------------------------------------------------------------

    #[test]
    fn pub_async_unsafe_const_fns_recognized() {
        let src = r#"
pub fn alpha() { let _ = triple_buffer.read(); }
async fn beta() { let _ = triple_buffer.read(); }
unsafe fn gamma() { let _ = triple_buffer.read(); }
const fn delta() -> u32 { 0 }
pub(crate) fn epsilon() { let _ = triple_buffer.read(); }
"#;
        let out = populate_from_sources(
            &[("mod.rs".to_string(), src.to_string())],
            &PopulatorConfig::default(),
        );
        for name in ["alpha", "beta", "gamma", "delta", "epsilon"] {
            assert!(
                out.function_to_id.contains_key(name),
                "expected {name} in function_to_id"
            );
        }
    }

    // ----------------------------------------------------------------
    // Custom config (extra entry name + pattern overrides)
    // ----------------------------------------------------------------

    #[test]
    fn custom_render_entry_names_match() {
        let src = r#"
fn alternate_paint_root() {
    // br-ft-x2oyy: intentional synthetic mutation fixture;
    // this dropped write result is test corpus data, not production logic.
    let _ = triple_buffer.write();
}
"#;
        let mut cfg = PopulatorConfig::default();
        cfg.render_entry_names = vec!["alternate_paint_root".to_string()];
        let out = populate_from_sources(&[("paint.rs".to_string(), src.to_string())], &cfg);
        assert_eq!(out.entry_points.len(), 1);
        assert_eq!(out.entry_points[0].function_name, "alternate_paint_root");
        let outcome = audit_render_call_graph(
            &out.graph,
            &out.entry_points,
            &out.guard_sites,
            AuditConfig::default(),
        );
        assert!(matches!(outcome, AuditOutcome::FailedWithViolations { .. }));
    }

    // ----------------------------------------------------------------
    // No render entry → audit pass even with mutations
    // ----------------------------------------------------------------

    #[test]
    fn audit_passes_when_no_render_entries_present() {
        let src = r#"
fn unrelated() {
    // br-ft-x2oyy: intentional synthetic mutation fixture;
    // this dropped write result is test corpus data, not production logic.
    let _ = triple_buffer.write();
}
"#;
        let out = populate_from_sources(
            &[("mod.rs".to_string(), src.to_string())],
            &PopulatorConfig::default(),
        );
        assert!(out.entry_points.is_empty());
        let outcome = audit_render_call_graph(
            &out.graph,
            &out.entry_points,
            &out.guard_sites,
            AuditConfig::default(),
        );
        assert_eq!(outcome, AuditOutcome::Pass);
    }

    // ----------------------------------------------------------------
    // Diagnostic data: file/line attribution
    // ----------------------------------------------------------------

    #[test]
    fn guard_site_records_file_and_line() {
        let src = "\nfn paint_impl() {\n    let _ = triple_buffer.read();\n}\n";
        // Lines:
        //   1: (empty)
        //   2: fn paint_impl() {
        //   3: let _ = triple_buffer.read();
        //   4: }
        let out = populate_from_sources(
            &[("paint.rs".to_string(), src.to_string())],
            &PopulatorConfig::default(),
        );
        assert_eq!(out.guard_sites.len(), 1);
        assert_eq!(out.guard_sites[0].file, "paint.rs");
        assert_eq!(out.guard_sites[0].line, 3);
    }

    // ----------------------------------------------------------------
    // Empty / whitespace-only input
    // ----------------------------------------------------------------

    #[test]
    fn empty_source_yields_empty_output() {
        let out = populate_from_sources(
            &[("empty.rs".to_string(), "".to_string())],
            &PopulatorConfig::default(),
        );
        assert!(out.entry_points.is_empty());
        assert!(out.guard_sites.is_empty());
        assert!(out.function_to_id.is_empty());
    }
}
