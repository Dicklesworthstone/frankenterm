//! Fuzz + property tests for the WASM extension sandbox hardened under ft-0dki4
//! (memory cap enforced + host-env leak closed). Bead: ft-3o4r0.
//!
//! Three security invariants of the sandbox boundary:
//!
//! 1. **Memory cap holds under pressure (property).** Sweep a range of declared
//!    linear-memory page counts: every module that clearly exceeds the per-module
//!    cap must be rejected by the `StoreLimits` resource limiter, and every
//!    clearly-under-cap module must get PAST the limiter (failing later only for
//!    lacking a config entrypoint). Proves the cap is a real threshold, not a
//!    single hard-coded check.
//! 2. **Malformed extension inputs never panic (fuzz).** A battery of crafted +
//!    pseudo-random byte blobs fed to the loader must each be rejected with an
//!    `Err` — never a panic (a panic aborts the test) and never silently accepted
//!    as a config. Guards loader robustness against adversarial input.
//! 3. **No host-env escape (regression guard).** The engine source must not use
//!    any host-inheritance WASI vector (env/stdio/args/network) — `inherit_env`
//!    leaked host secrets into the guest `environ` before ft-0dki4. (`.inherit_stderr()`
//!    for diagnostics is intentionally allowed and excluded.)
//!
//! Gated on `--features wasm` (the engine only exists with wasmtime). Zero-RCH:
//! real public API, no mocks, pure in-process + tempdir.

#![cfg(feature = "wasm")]

use frankenterm_scripting::{ScriptingEngine, WasmEngine};

// --- minimal raw-WASM builders (mirrors wasm_engine.rs test helpers) ----------

/// ULEB128-encode a u32 into `out`.
fn push_uleb_u32(out: &mut Vec<u8>, mut value: u32) {
    loop {
        let mut byte = u8::try_from(value & 0x7f).expect("ULEB128 chunk fits in u8");
        value >>= 7;
        if value == 0 {
            out.push(byte);
            break;
        }
        byte |= 0x80;
        out.push(byte);
    }
}

/// Append a WASM section: `id` + ULEB(len) + payload.
fn push_section(module: &mut Vec<u8>, section_id: u8, payload: &[u8]) {
    module.push(section_id);
    push_uleb_u32(module, u32::try_from(payload.len()).expect("section length fits in u32"));
    module.extend_from_slice(payload);
}

/// A valid WASM module declaring a single linear memory with `pages` minimum
/// pages (64 KiB each) and no other content.
fn module_with_memory_min(pages: u32) -> Vec<u8> {
    let mut module = b"\0asm\x01\0\0\0".to_vec();
    let mut mem = Vec::new();
    push_uleb_u32(&mut mem, 1); // one memory
    mem.push(0x00); // limits flag: min only
    push_uleb_u32(&mut mem, pages);
    push_section(&mut module, 5, &mem); // section id 5 = Memory
    module
}

fn mentions_memory_limit(err_debug: &str) -> bool {
    let m = err_debug.to_lowercase();
    m.contains("memor") || m.contains("limit") || m.contains("exceed")
}

// --- 1. memory cap holds under pressure (property) ----------------------------

#[test]
fn wasm_memory_cap_holds_across_page_counts_property_ft_0dki4() {
    let engine = WasmEngine::with_defaults().expect("engine"); // 64 MiB cap = 1024 pages
    let dir = tempfile::tempdir().expect("tempdir");

    // Clearly OVER the cap (>= 2x): must be rejected BY THE MEMORY LIMITER.
    for &pages in &[2048_u32, 4096, 16384] {
        let path = dir.path().join(format!("over_{pages}.wasm"));
        std::fs::write(&path, module_with_memory_min(pages)).expect("write");
        let err = engine
            .eval_config(&path)
            .expect_err("over-cap module must be rejected at instantiation");
        assert!(
            mentions_memory_limit(&format!("{err:?}")),
            "ft-0dki4: {pages} pages (> 64 MiB cap) must fail on the memory limiter, got: {err:?}"
        );
    }

    // Clearly UNDER the cap: must get PAST the limiter and fail only for lacking a
    // config entrypoint (NOT for memory). If the limiter were dropped, the over-cap
    // modules above would also reach this path and that loop would fail — so the
    // two halves together prove the limiter rejects on SIZE, not unconditionally.
    for &pages in &[1_u32, 16, 256, 1000] {
        let path = dir.path().join(format!("under_{pages}.wasm"));
        std::fs::write(&path, module_with_memory_min(pages)).expect("write");
        let err = engine
            .eval_config(&path)
            .expect_err("under-cap module still lacks a config entrypoint");
        assert!(
            !mentions_memory_limit(&format!("{err:?}")),
            "ft-0dki4: {pages} pages (< 64 MiB cap) must PASS the memory limiter, got: {err:?}"
        );
    }
}

// --- 2. malformed extension inputs never panic (fuzz) -------------------------

#[test]
fn wasm_loader_rejects_malformed_inputs_without_panic_fuzz() {
    let engine = WasmEngine::with_defaults().expect("engine");
    let dir = tempfile::tempdir().expect("tempdir");

    let mut inputs: Vec<Vec<u8>> = vec![
        Vec::new(),                                // empty
        b"\0asm".to_vec(),                         // magic only, truncated version
        b"\0asm\x01\0\0\0".to_vec(),               // valid header, no config export
        b"AAAA\x01\0\0\0".to_vec(),                // wrong magic
        b"\0asm\xff\xff\xff\xff".to_vec(),         // bad version
        vec![0x05, 0xff, 0xff, 0xff, 0xff, 0x0f],  // section id 5 + huge ULEB len, no body
        {
            // valid header + memory section claiming a payload that isn't present
            let mut m = b"\0asm\x01\0\0\0".to_vec();
            m.push(0x05); // memory section id
            m.push(0x7f); // claims 127 payload bytes that don't exist
            m.push(0x01);
            m
        },
    ];

    // Deterministic pseudo-random blobs of varying length (LCG — no wall clock,
    // no RNG, so the corpus is stable across runs).
    let mut state: u32 = 0x1234_5678;
    for i in 0..64_u32 {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let len = usize::try_from(state % 257).expect("modulo fits usize") + 1;
        let mut blob = Vec::with_capacity(len);
        let mut s = state ^ i;
        for _ in 0..len {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            blob.push(u8::try_from(s >> 24).expect("u32 >> 24 fits u8"));
        }
        inputs.push(blob);
    }

    for (idx, bytes) in inputs.iter().enumerate() {
        let path = dir.path().join(format!("fuzz_{idx}.wasm"));
        std::fs::write(&path, bytes).expect("write");
        // A panic inside eval_config would abort the test → failure. Reaching the
        // assert means the loader handled the input gracefully.
        let result = engine.eval_config(&path);
        assert!(
            result.is_err(),
            "fuzz input #{idx} ({} bytes) must be rejected, not accepted as config",
            bytes.len()
        );
    }
}

// --- 3. no host-env escape (regression guard) ---------------------------------

#[test]
fn wasm_engine_has_no_host_inheritance_leak_vectors_ft_0dki4() {
    // Read the engine source and assert it uses NO host-inheritance WASI vector.
    // This test lives in a separate file from the engine, so the needles below
    // cannot match their own assertion text. `.inherit_stderr()` (diagnostics
    // only, not a secret-bearing channel) is intentionally NOT in the deny list.
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/wasm_engine.rs"
    ))
    .expect("read wasm_engine.rs source");

    for needle in [
        ".inherit_env(",     // host env -> guest environ (the ft-0dki4 leak)
        ".inherit_stdio(",   // re-exposes host stdin/stdout/stderr in bulk
        ".inherit_args(",    // host argv (may carry secrets) -> guest
        ".inherit_network(", // ambient host network access
    ] {
        assert!(
            !src.contains(needle),
            "ft-0dki4 regression: WASI ctx must not use host-inheritance vector {needle:?}"
        );
    }

    // Positive guard: the per-store memory limiter must still be installed (so
    // this file also fails closed if the cap enforcement is removed).
    assert!(
        src.contains("StoreLimitsBuilder")
            && src.contains(".memory_size(")
            && src.contains(".limiter("),
        "ft-0dki4 regression: create_store must install a StoreLimits memory limiter"
    );
}
