//! Regression guard for the mlua 0.11 `block_on` deadlock class
//! (ft-3dz22 / ft-mlua-fix epic).
//!
//! The mlua 0.9 -> 0.11 migration converted async Lua API functions to *sync*
//! `create_function` / `add_method` closures that wrapped the awaited future in
//! `promise::spawn::block_on(..)`. When such a closure runs on the main-thread
//! spawn queue -- i.e. invoked from a `gui-startup` handler, an event handler,
//! or a keybinding -- `promise::spawn::block_on` trips the main-thread dispatch
//! deadlock guard (see `promise::spawn::assert_not_in_main_thread_dispatch`)
//! and SIGABRTs the GUI on launch.
//!
//! The correct shapes are `create_async_function` / `add_async_method` that
//! either `.await` a `Send` future directly, or hand `!Send` work to the
//! main-thread queue via `promise::spawn::spawn(..).await` (which uses
//! `spawn_local`, lifting the `Send` bound, and yields to the event loop
//! instead of blocking). Self-contained, non-dispatching work may use a private
//! executor such as `futures::executor::block_on`.
//!
//! This test walks every production `src/` `*.rs` file across the
//! `frankenterm/lua-api-crates/` workspace folder and fails if any *code*
//! (line comments stripped) references `promise::spawn::block_on`. It is a
//! cheap, build-free grep so it cannot itself reintroduce the deadlock.

use std::fs;
use std::path::{Path, PathBuf};

/// Strip a naive `//` line comment from a source line.
///
/// This is intentionally simple: the banned path token never legitimately
/// appears inside a string literal in these crates, and the only thing we must
/// avoid matching is the *explanatory* comments (in the very files we fixed)
/// that mention `promise::spawn::block_on` while describing why it is banned.
fn strip_line_comment(line: &str) -> &str {
    match line.find("//") {
        Some(idx) => &line[..idx],
        None => line,
    }
}

/// Collect every `.rs` file that lives under a `src/` directory beneath `dir`.
///
/// Restricting to `src/` keeps the guard focused on production Lua-callback code
/// (the only place the deadlock manifests) and naturally excludes this test file
/// itself, which lives under `tests/`.
fn collect_src_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            match path.file_name().and_then(|n| n.to_str()) {
                // Never descend into build output or non-production trees.
                Some("target") | Some("tests") | Some("benches") | Some("examples") => continue,
                _ => collect_src_rs_files(&path, out),
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs")
            && path.components().any(|c| c.as_os_str() == "src")
        {
            out.push(path);
        }
    }
}

#[test]
fn no_promise_block_on_in_lua_api_crates() {
    // CARGO_MANIFEST_DIR is `.../frankenterm/lua-api-crates/mux-lua`; its parent
    // is the `lua-api-crates` folder holding every Lua API crate.
    let lua_api_crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("mux-lua crate dir has a parent (lua-api-crates)")
        .to_path_buf();

    let mut files = Vec::new();
    collect_src_rs_files(&lua_api_crates, &mut files);
    assert!(
        !files.is_empty(),
        "guard found no src/*.rs files under {} -- path resolution broke; \
         the guard would silently pass and stop protecting against the \
         block_on deadlock regression",
        lua_api_crates.display()
    );

    // Build the banned token at runtime so this very file contains no literal
    // copy of it in *code* (only in the doc comment above, which is stripped).
    let banned = ["promise", "spawn", "block_on"].join("::");

    let mut offenders = Vec::new();
    for file in &files {
        let text = match fs::read_to_string(file) {
            Ok(t) => t,
            Err(_) => continue,
        };
        for (idx, line) in text.lines().enumerate() {
            if strip_line_comment(line).contains(&banned) {
                offenders.push(format!("{}:{}: {}", file.display(), idx + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "{banned} must not be called from any lua-api-crates `src/` file: it \
         trips the main-thread spawn-queue dispatch deadlock guard when a Lua \
         callback runs from a gui-startup / event / keybinding handler, \
         aborting the GUI on launch. Use create_async_function / \
         add_async_method with `.await`, or hand !Send work to \
         `{spawn}(..).await`. Offending site(s):\n{sites}",
        spawn = ["promise", "spawn", "spawn"].join("::"),
        sites = offenders.join("\n"),
    );
}
