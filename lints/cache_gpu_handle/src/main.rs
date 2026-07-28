//! CLI runner for the cache-gpu-handle lint.
//!
//! **Class:** glyph-run-interner atlas-pinning GPU-memory leak.
//!
//! # Usage
//!
//! ```text
//! cargo run -p cache_gpu_handle_lint -- crates/frankenterm-gui/src frankenterm/window/src
//! ```
//!
//! One or more SRC_ROOT directories may be passed; the type graph is
//! built from their union so a process-global in one dir can be caught
//! reaching a forbidden leaf defined in another.
//!
//! Exit code 0 = clean (no process-global cache can pin a GPU resource).
//! Exit code 1 = at least one process-global container transitively
//! owns a GPU-handle leaf (the leak class). Exit code 2 = bad args.
//!
//! Pass `--json` for a machine-readable summary on stdout.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let mut emit_json = false;
    let mut src_roots: Vec<PathBuf> = Vec::new();
    for arg in &args[1..] {
        if arg == "--json" {
            emit_json = true;
        } else if arg == "--help" || arg == "-h" {
            print_help();
            return ExitCode::SUCCESS;
        } else if !arg.starts_with("--") {
            src_roots.push(PathBuf::from(arg));
        }
    }

    if src_roots.is_empty() {
        src_roots.push(PathBuf::from("crates/frankenterm-gui/src"));
        src_roots.push(PathBuf::from("frankenterm/window/src"));
    }

    for root in &src_roots {
        if !root.is_dir() {
            eprintln!(
                "cache-gpu-handle-lint: src root `{}` is not a directory",
                root.display()
            );
            return ExitCode::from(2);
        }
    }

    let report = match cache_gpu_handle_lint::audit_dirs(&src_roots) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cache-gpu-handle-lint: walk failed: {e}");
            return ExitCode::from(2);
        }
    };

    if emit_json {
        emit_json_report(&report);
    } else {
        emit_human_report(&report);
    }

    if report.findings.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn print_help() {
    println!(
        "cache-gpu-handle-lint — forbid process-global caches from owning a GPU-resource handle"
    );
    println!();
    println!("usage: cache-gpu-handle-lint [--json] [SRC_ROOT ...]");
    println!();
    println!("  SRC_ROOT  default: crates/frankenterm-gui/src frankenterm/window/src");
    println!("  --json    emit machine-readable summary on stdout");
    println!();
    println!("Forbidden GPU-handle leaves: Sprite, CachedGlyph, Texture2d, WebGpuTexture, wgpu::Texture, wgpu::TextureView");
}

fn emit_human_report(report: &cache_gpu_handle_lint::AuditReport) {
    if report.findings.is_empty() {
        println!(
            "cache-gpu-handle-lint: clean. {} process-global container(s) scanned ({} clean, {} allow-listed), {} type-graph node(s).",
            report.total_globals,
            report.clean_globals,
            report.allow_listed_globals,
            report.type_graph_nodes,
        );
        return;
    }

    eprintln!(
        "cache-gpu-handle-lint: {} finding(s) — process-global cache(s) can pin a GPU resource:",
        report.findings.len()
    );
    for f in &report.findings {
        eprintln!("  {}", f.render());
    }
    eprintln!();
    eprintln!(
        "Summary: {} process-global container(s) scanned, {} reach a GPU-handle leaf, {} clean, {} allow-listed, {} type-graph node(s).",
        report.total_globals,
        report.finding_count(),
        report.clean_globals,
        report.allow_listed_globals,
        report.type_graph_nodes,
    );
    eprintln!(
        "Fix: cache the atlas-INVARIANT slice (positions / metrics, no Rc<CachedGlyph>) and re-attach the live glyph on hit — see shapecache.rs ShapedInfoTemplate."
    );
}

fn emit_json_report(report: &cache_gpu_handle_lint::AuditReport) {
    print!("{{");
    print!("\"total_globals\":{}", report.total_globals);
    print!(",\"clean_globals\":{}", report.clean_globals);
    print!(",\"allow_listed_globals\":{}", report.allow_listed_globals);
    print!(",\"type_graph_nodes\":{}", report.type_graph_nodes);
    print!(",\"finding_count\":{}", report.finding_count());
    print!(",\"findings\":[");
    let mut first = true;
    for f in &report.findings {
        if !first {
            print!(",");
        }
        first = false;
        print!(
            "{{\"path\":{:?},\"line\":{},\"global\":{:?},\"root_type\":{:?},\"forbidden_leaf\":{:?},\"path_witness\":{:?}}}",
            f.rel_path.display().to_string(),
            f.line,
            f.global,
            f.root_type,
            f.forbidden_leaf,
            f.path.join(" -> "),
        );
    }
    println!("]}}");
}
