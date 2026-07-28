//! Consolidated session security-conformance suite — scripting crate (ft-bkn2g).
//!
//! ZERO-RCH. Covers the scripting-crate security fixes from the session:
//!   * ft-a58cq — extension manifest names are validated before any filesystem
//!     join, blocking path-traversal/absolute/separator escapes (behavioral,
//!     public API).
//!   * ft-0dki4 — the WASM engine enforces a memory cap and does not leak the
//!     host environment into the guest (wiring guard; the behavioral
//!     memory-limit test lives inline in wasm_engine.rs and needs `--features
//!     wasm`, so this cross-cutting gate is a source check that holds without
//!     the feature).
//!
//! Core-crate fixes (ft-l59nq, ft-pfton, ft-cdxrr, ft-ps9fu) are covered by the
//! sibling suite crates/frankenterm-core/tests/session_security_conformance.rs.

use std::path::Path;

use frankenterm_scripting::manifest::validate_extension_name;

// ---------------------------------------------------------------------------
// ft-a58cq — extension name path-traversal guard
// ---------------------------------------------------------------------------

#[test]
fn ft_a58cq_extension_name_rejects_filesystem_escapes() {
    // Names that could steer create/remove/extract outside the extensions root.
    let rejected = [
        "../outside",
        "..",
        ".",
        "a/b",
        "/abs",
        "\\abs",
        "a\\b",
        "foo/../bar",
        "",
        " leading",
        "trailing ",
        "C:evil", // Windows drive prefix
        "a:b",    // colon (and drive-like)
        "semi;colon",
        "with space",
    ];
    for name in rejected {
        assert!(
            validate_extension_name(name).is_err(),
            "ft-a58cq regression: unsafe extension name must be rejected: {name:?}"
        );
    }

    // Legitimate single-component names still pass.
    for name in ["my-extension", "ext_1", "Extension2", "a.b.c", "x-y_z.1"] {
        assert!(
            validate_extension_name(name).is_ok(),
            "valid extension name must be accepted: {name:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// ft-0dki4 — WASM sandbox: memory cap installed, host env not inherited
// ---------------------------------------------------------------------------

fn read_scripting_src(rel: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn brace_delta(line: &str) -> i32 {
    let opens = line.bytes().filter(|&b| b == b'{').count() as i32;
    let closes = line.bytes().filter(|&b| b == b'}').count() as i32;
    opens - closes
}

/// Strip `#[cfg(test)]` items so the guard reflects only production code.
fn production_only(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut armed = false;
    let mut in_test = false;
    let mut depth: i32 = 0;
    for line in src.lines() {
        if !in_test && !armed && line.trim_start().starts_with("#[cfg(test)]") {
            armed = true;
            continue;
        }
        if armed {
            if line.contains('{') {
                depth += brace_delta(line);
                if depth > 0 {
                    in_test = true;
                } else {
                    depth = 0;
                }
                armed = false;
                continue;
            }
            if line.trim_end().ends_with(';') {
                armed = false;
            }
            continue;
        }
        if in_test {
            depth += brace_delta(line);
            if depth <= 0 {
                in_test = false;
                depth = 0;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[test]
fn ft_0dki4_wasm_sandbox_caps_memory_and_does_not_leak_env() {
    let src = production_only(&read_scripting_src("wasm_engine.rs"));
    // Needles assembled from fragments so this guard never matches its own
    // assertion text (it scans a different file, but stay defensive).
    let inherit_env = concat!(".inherit", "_env(");
    let limiter = concat!(".limit", "er(");
    let limits_builder = concat!("StoreLimits", "Builder");

    assert!(
        !src.contains(inherit_env),
        "ft-0dki4 regression: WASM WASI ctx must NOT inherit the host environment \
         (leaks host secrets into the guest)"
    );
    assert!(
        src.contains(limiter) && src.contains(limits_builder),
        "ft-0dki4 regression: create_store must install a StoreLimits memory limiter \
         (otherwise a module can grow to the wasm32 4 GiB ceiling)"
    );
}
