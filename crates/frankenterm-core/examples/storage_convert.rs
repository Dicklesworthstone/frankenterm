//! `ft storage convert` reference CLI (ft-s03ox).
//!
//! Drives the substrate's [`storage_backend_converter::convert_db`]
//! and [`storage_backend_converter::verify_equivalence`] over two
//! `RusqliteBackend` opens — the source database is read, every
//! named table copied row-by-row into the destination via the
//! trait abstraction, and (with `--verify`) byte-level equivalence
//! is checked after the copy.
//!
//! When `frankensqlite` lands at ft-kcdqp the backends_under_test
//! table here grows a new variant; until then both ends speak
//! `RusqliteBackend` and the example doubles as a vacuum / WAL-
//! snapshot tool the operator runbook references.
//!
//! Usage:
//!   cargo run --example storage_convert -p frankenterm-core \
//!     --no-default-features -- \
//!     --source /path/to/source.db \
//!     --dest   /path/to/dest.db \
//!     [--tables panes,events,...] \
//!     [--verify] \
//!     [--allow-overwrite]
//!
//! Output is a single JSON object on stdout describing the run:
//!   { "source": ..., "dest": ..., "tables": [...],
//!     "rows_per_table": [...], "total_rows": N,
//!     "verified": bool, "elapsed_ms": M }
//!
//! Exit codes:
//!   0 — convert (and optional verify) succeeded.
//!   1 — argument or IO error.
//!   2 — verify_equivalence rejected the dest (table / row / cell
//!       coordinates surfaced in stderr).

use std::collections::BTreeSet;
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use frankenterm_core::storage_backend_converter::{convert_db, verify_equivalence};
use frankenterm_core::storage_backend_trait::{OpenConfig, RusqliteBackend, StorageBackend};

#[derive(Debug)]
struct Args {
    source: String,
    dest: String,
    tables: Option<Vec<String>>,
    verify: bool,
    allow_overwrite: bool,
}

fn print_usage() {
    eprintln!(
        "{}",
        "Usage: storage_convert --source <path> --dest <path> [--tables a,b,c] [--verify] [--allow-overwrite]"
    );
}

fn parse_args(raw: Vec<String>) -> Result<Args, String> {
    let mut source: Option<String> = None;
    let mut dest: Option<String> = None;
    let mut tables: Option<Vec<String>> = None;
    let mut verify = false;
    let mut allow_overwrite = false;
    let mut iter = raw.into_iter().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--source" => {
                source = Some(iter.next().ok_or("--source needs a value")?);
            }
            "--dest" => {
                dest = Some(iter.next().ok_or("--dest needs a value")?);
            }
            "--tables" => {
                let raw = iter.next().ok_or("--tables needs a value")?;
                let parsed: Vec<String> = raw
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
                tables = Some(parsed);
            }
            "--verify" => verify = true,
            "--allow-overwrite" => allow_overwrite = true,
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Args {
        source: source.ok_or("--source is required")?,
        dest: dest.ok_or("--dest is required")?,
        tables,
        verify,
        allow_overwrite,
    })
}

/// Discover the user-visible table set on `backend` by walking
/// `sqlite_master`. Drops sqlite-internal tables (`sqlite_*`) so
/// the conversion does not try to copy bookkeeping rows.
fn discover_tables(backend: &dyn StorageBackend) -> Result<Vec<String>, String> {
    let rows = backend
        .query_map_strings(
            "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name",
            &[],
        )
        .map_err(|e| format!("discover tables: {e}"))?;
    let mut out: Vec<String> = rows
        .into_iter()
        .filter_map(|r| r.into_iter().next())
        .filter(|name| !name.starts_with("sqlite_"))
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

/// Pull the `CREATE TABLE` / `CREATE INDEX` / `CREATE TRIGGER`
/// statements from the source's sqlite_master and execute them on
/// the destination so [`convert_db`] has the schema it needs to
/// land rows. Skips sqlite-internal entries.
///
/// The substrate's `convert_db` only copies rows — schema
/// replication is the wired-pass responsibility cc_1's substrate-
/// pass commit called out. Doing it here in the reference CLI
/// keeps the operator-facing flow ("source → dest") usable
/// without a separate schema-bootstrap step.
fn replicate_schema(
    source: &dyn StorageBackend,
    dest: &dyn StorageBackend,
) -> Result<usize, String> {
    let rows = source
        .query_map_strings(
            "SELECT sql FROM sqlite_master \
             WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' \
             ORDER BY \
                 CASE type WHEN 'table' THEN 0 \
                           WHEN 'index' THEN 1 \
                           WHEN 'view'  THEN 2 \
                           WHEN 'trigger' THEN 3 \
                           ELSE 4 END, \
                 name",
            &[],
        )
        .map_err(|e| format!("read sqlite_master: {e}"))?;
    let mut count = 0;
    for row in rows {
        let Some(stmt) = row.into_iter().next() else {
            continue;
        };
        if stmt.is_empty() {
            continue;
        }
        dest.execute_batch(&stmt)
            .map_err(|e| format!("apply schema `{stmt}`: {e}"))?;
        count += 1;
    }
    Ok(count)
}

fn run(args: Args) -> Result<i32, String> {
    if !Path::new(&args.source).exists() {
        return Err(format!("source `{}` does not exist", args.source));
    }
    if Path::new(&args.dest).exists() && !args.allow_overwrite {
        return Err(format!(
            "dest `{}` already exists (rerun with --allow-overwrite to clobber)",
            args.dest
        ));
    }
    let source = RusqliteBackend::open(&args.source, &OpenConfig::default())
        .map_err(|e| format!("open source: {e}"))?;
    let dest = RusqliteBackend::open(&args.dest, &OpenConfig::default())
        .map_err(|e| format!("open dest: {e}"))?;

    let tables: Vec<String> = match args.tables {
        Some(explicit) => {
            // De-duplicate while preserving order.
            let mut seen: BTreeSet<String> = BTreeSet::new();
            explicit
                .into_iter()
                .filter(|t| seen.insert(t.clone()))
                .collect()
        }
        None => discover_tables(&source)?,
    };
    if tables.is_empty() {
        return Err(
            "no tables to copy (source database has no user tables; pass --tables to override)"
                .to_string(),
        );
    }

    let table_refs: Vec<&str> = tables.iter().map(String::as_str).collect();
    let started = Instant::now();
    let schema_objects = replicate_schema(&source, &dest)?;
    let outcome = convert_db(&source, &dest, &table_refs)
        .map_err(|e| format!("convert_db: {e}"))?;
    let mut verify_ok = None;
    let mut verify_err: Option<String> = None;
    if args.verify {
        match verify_equivalence(&source, &dest, &table_refs) {
            Ok(()) => verify_ok = Some(true),
            Err(e) => {
                verify_ok = Some(false);
                verify_err = Some(format!("{e}"));
            }
        }
    }
    let elapsed_ms = started.elapsed().as_millis() as u64;

    let payload = serde_json::json!({
        "bead": "ft-s03ox",
        "schema_version": 1,
        "source": args.source,
        "dest": args.dest,
        "schema_objects_replicated": schema_objects,
        "tables": outcome.tables,
        "rows_per_table": outcome.rows_per_table,
        "total_rows": outcome.total_rows,
        "verified": verify_ok,
        "verify_error": verify_err,
        "elapsed_ms": elapsed_ms,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string())
    );

    match verify_ok {
        Some(false) => Ok(2),
        _ => Ok(0),
    }
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().collect();
    let args = match parse_args(raw) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            print_usage();
            return ExitCode::from(1);
        }
    };
    match run(args) {
        Ok(0) => ExitCode::SUCCESS,
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}
