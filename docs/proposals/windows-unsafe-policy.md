# Windows Support: Unsafe Policy (ft-f6oi0)

**Epic:** ft-azsnz — make `ft` build and run on Windows without changing
Mac/Linux behavior.

**Status:** Decided. This document is the binding policy for all Windows
platform code added under ft-azsnz.

## The invariant

`#![forbid(unsafe_code)]` stays on every crate that currently declares it —
notably `frankenterm-core` and `frankenterm-alloc`. Windows support **must not**
weaken this. A `forbid(unsafe_code)` crate that needs platform behavior obtains
it through a **safe** dependency surface, never by relaxing the lint.

## Decision: zero unsafe in first-party code

Windows platform behavior is implemented exclusively through safe third-party
surfaces:

| Need | Safe surface | Notes |
|------|--------------|-------|
| process / CPU / disk / memory pressure | [`sysinfo`](https://crates.io/crates/sysinfo) | Cross-platform, fully safe API. Already the intended backend for the pressure seams (cpu_pressure / disk_pressure / memory_pressure). |
| Windows API odds-and-ends (paths, console, errors) | [`windows`](https://crates.io/crates/windows) **safe surfaces only** | The `windows` crate exposes many safe wrappers; use those. Do **not** call its raw `unsafe` FFI from a `forbid(unsafe_code)` crate. |
| IPC / control socket (the Unix `UnixStream`/`UnixListener` analogue) | a safe named-pipe crate (e.g. `interprocess`, or `tokio`-free named-pipe wrappers) | Provides a safe `connect`/`listen` surface over Win32 named pipes. |

When every Windows need is met by a safe surface, **first-party code carries
zero `unsafe`** and the `forbid(unsafe_code)` lint stays everywhere it is today.

## Escape hatch (only if no safe wrapper exists)

If — and only if — a required Win32 capability has **no** safe wrapper in any
maintained crate, the raw FFI is isolated in a dedicated
`#[cfg(windows)]`-only helper crate (e.g. `frankenterm-win-sys`) that:

1. Is the **single** place in the workspace permitted to omit
   `#![forbid(unsafe_code)]`. It instead carries `#![deny(unsafe_op_in_unsafe_fn)]`
   and documents every `unsafe` block with its safety contract.
2. Exposes a **100% safe public API**; callers (the `forbid(unsafe_code)`
   crates) only ever see safe functions.
3. Is `#[cfg(windows)]`-gated end-to-end, so it does not exist in the Unix
   dependency graph at all.
4. Is kept minimal and reviewed as security-sensitive code.

This escape hatch is expected to stay **empty**: the sysinfo + windows-safe +
named-pipe surfaces above are projected to cover the ft-azsnz scope (pressure
seams, IPC, process/env). The helper crate is created lazily, only when a
concrete gap is proven.

## Additive-only constraint (epic-wide)

- New platform behavior is added as `#[cfg(windows)]` branches behind platform
  **trait seams**. The existing Unix/macOS implementation is a **literal move**
  of current code behind the seam — **no logic change** — so Mac/Linux behavior
  is byte-for-byte unchanged.
- No new **unconditional** `std::os::unix` usage; anything Unix-specific moves
  behind `#[cfg(unix)]` (or the seam's unix impl).
- **Guardrail:** `cargo check --workspace` on Unix stays green, and
  `cargo check --workspace --target x86_64-pc-windows-msvc` stays green (now a
  required CI gate). Local cross-check without a Windows box:
  `rustup target add x86_64-pc-windows-msvc`, then
  `CARGO_TARGET_DIR=/tmp/ft-swarm-p2-wincheck cargo check -p <crate> --target x86_64-pc-windows-msvc`
  (compiles to Windows metadata; no link/run). Scope to `-p <crate>` — do not
  rebuild the whole workspace (disk is tight).

## Why not raw `windows`/`winapi` FFI in core?

`frankenterm-core` is the security- and correctness-critical surface (policy,
redaction, auth, capture). Its `forbid(unsafe_code)` guarantee is a load-bearing
part of the threat model — it means no first-party memory-safety footgun can
exist in that crate. Threading raw Win32 `unsafe` through it would erode that
guarantee for the entire platform. The safe-surface approach keeps the guarantee
intact at the cost of a dependency, which is the right trade for this codebase.
