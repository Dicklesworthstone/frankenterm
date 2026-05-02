//! CLI runner for the cx-propagation lint.
//!
//! **Bead:** ft-t9a6q.1. **Doc:** docs/runtime/cx-propagation-lint.md.
//!
//! # Usage
//!
//! ```text
//! cargo run -p cx_propagation_lint -- crates/frankenterm-core/src
//! ```
//!
//! Exit code 0 = clean (no findings). Exit code 1 = at least one
//! `pub async fn` is missing `&Cx` / `RuntimeProof`. Each finding
//! prints to stderr in `path:line: reason` shape so editors can
//! navigate to the call site.
//!
//! Pass `--json` to emit a machine-readable summary on stdout
//! (used by the burn-down dashboard under ft-t9a6q.2).

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let mut emit_json = false;
    let mut src_root: Option<PathBuf> = None;
    for arg in &args[1..] {
        if arg == "--json" {
            emit_json = true;
        } else if arg == "--help" || arg == "-h" {
            print_help();
            return ExitCode::SUCCESS;
        } else if !arg.starts_with("--") {
            src_root = Some(PathBuf::from(arg));
        }
    }

    let src_root = src_root.unwrap_or_else(|| PathBuf::from("crates/frankenterm-core/src"));
    if !src_root.is_dir() {
        eprintln!(
            "cx-propagation-lint: src root `{}` is not a directory",
            src_root.display()
        );
        return ExitCode::from(2);
    }

    let report = match cx_propagation_lint::audit_dir(&src_root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "cx-propagation-lint: failed to walk `{}`: {e}",
                src_root.display()
            );
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
    println!("cx-propagation-lint — flag pub async fns missing &Cx / RuntimeProof");
    println!();
    println!("usage: cx-propagation-lint [--json] [SRC_ROOT]");
    println!();
    println!("  SRC_ROOT  default: crates/frankenterm-core/src");
    println!("  --json    emit machine-readable summary on stdout");
    println!();
    println!("Doc: docs/runtime/cx-propagation-lint.md");
    println!("Bead: ft-t9a6q.1");
}

fn emit_human_report(report: &cx_propagation_lint::AuditReport) {
    if report.findings.is_empty() {
        println!(
            "cx-propagation-lint: clean. {} pub async fn sites scanned ({} covered, {} exempt-file, {} wrapper-exempt).",
            report.total_pub_async_sites,
            report.covered_sites,
            report.exempt_file_sites,
            report.wrapper_exempt_sites,
        );
        return;
    }

    eprintln!("cx-propagation-lint: {} finding(s):", report.findings.len());
    for f in &report.findings {
        eprintln!("  {}", f.render());
    }
    eprintln!();
    eprintln!(
        "Summary: {} pub async fn sites scanned, {} uncovered, {} stale exemption(s), {} covered, {} exempt-file, {} wrapper-exempt.",
        report.total_pub_async_sites,
        report.uncovered_count(),
        report.stale_exemption_count(),
        report.covered_sites,
        report.exempt_file_sites,
        report.wrapper_exempt_sites,
    );
}

fn emit_json_report(report: &cx_propagation_lint::AuditReport) {
    // Hand-rolled JSON to avoid a serde dependency in the lint
    // crate. The shape mirrors the Python audit script's --json
    // output so the burn-down dashboard (ft-t9a6q.2) can consume
    // either source interchangeably.
    print!("{{");
    print!("\"total_pub_async_sites\":{}", report.total_pub_async_sites);
    print!(",\"exempt_file_sites\":{}", report.exempt_file_sites);
    print!(",\"wrapper_exempt_sites\":{}", report.wrapper_exempt_sites);
    print!(",\"covered_sites\":{}", report.covered_sites);
    print!(",\"uncovered_sites\":{}", report.uncovered_count());
    print!(
        ",\"stale_exemption_sites\":{}",
        report.stale_exemption_count()
    );
    print!(",\"findings\":[");
    let mut first = true;
    for f in &report.findings {
        if !first {
            print!(",");
        }
        first = false;
        let kind = match f.reason {
            cx_propagation_lint::FindingReason::MissingCxOrRuntimeProof => "missing_cx",
            cx_propagation_lint::FindingReason::StaleWrapperExemption => "stale_exemption",
        };
        print!(
            "{{\"path\":{:?},\"line\":{},\"fn_name\":{:?},\"reason\":{:?}}}",
            f.rel_path.display().to_string(),
            f.line,
            f.fn_name,
            kind,
        );
    }
    print!("]}}\n");
}
