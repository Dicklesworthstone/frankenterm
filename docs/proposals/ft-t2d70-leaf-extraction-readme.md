# ft-t2d70 leaf-extraction plan — STATUS: paused for review

**Bead context:** ft-t2d70 closed with PARK ADR. Three child beads filed under
ft-t2d70 to stage the prerequisite leaf-types extractions. **The first child
(ft-g6sa8 = .1) was claimed but extraction is paused** because the underlying
files are larger and more entangled than the parent ADR's "~117 LOC"
estimate suggested. This README explains what I found and what should happen
next.

## Children filed

| Bead       | Title                                                           | Estimated effort                |
| ---------- | --------------------------------------------------------------- | ------------------------------- |
| ft-g6sa8   | [ft-t2d70.1] extract `frankenterm-core-error-types` leaf crate  | **NOT 117 LOC — see below**     |
| ft-otfxs   | [ft-t2d70.2] extract `frankenterm-core-config-types` leaf crate | ~1600 LOC, big                  |
| ft-0pykm   | [ft-t2d70.3] extract `frankenterm-core-policy-types` leaf crate | ~1500 LOC + entangled telemetry |

## What's actually in `error.rs`

`crates/frankenterm-core/src/error.rs` is **1,486 lines**, not 117. The "117"
in the parent proposal was the line *number* of `pub type Result<T>` — I
misread my own grep output when writing the ADR.

Top-level definitions:

```
struct  RemediationCommand                  (line   8, leaf-clean)
struct  Remediation                         (line  19, leaf-clean)
type    Result<T>                           (line 117, leaf-clean)
enum    RuntimeOperationSource              (line 121, leaf-clean)
enum    PaneOperationSource                 (line 132, leaf-clean)
enum    WatchdogWarningSource               (line 143, leaf-clean)
enum    Error                               (line 151, USES network_reliability)
enum    WeztermError                        (line 357, USES network_reliability)
enum    StorageError                        (line 487, USES network_reliability)
enum    PatternError                        (line 583, leaf-clean variants)
enum    WorkflowError                       (line 627, leaf-clean variants)
enum    ConfigError                         (line 669, leaf-clean variants)
fn      format_error_with_remediation       (line 724, top-level)
```

The three load-bearing enums consumed by `mcp_error.rs` (`Error`,
`StorageError`, `WeztermError`) carry inherent methods like:

```rust
impl Error {
    pub fn error_kind(&self) -> crate::network_reliability::NetworkErrorKind {
        // ...calls crate::network_reliability::classify_io_error...
    }
}
```

There are **16 references to `crate::network_reliability`** inside `error.rs`
method bodies. Rust's orphan rule for inherent impls means the impl block
must live in the same crate as the type definition — so to move `Error`
into a leaf crate, *every* `impl Error { … }` block must move too, which
brings the `network_reliability` dep with them, breaking the leaf property.

## What that means for the prerequisite chain

The "easiest leaf" framing in the parent ADR is wrong. The actual
prerequisite work is one of:

- **(A) Sub-leaf first.** Extract a `frankenterm-core-network-classification`
  leaf containing the `NetworkErrorKind` enum + `classify_io_error` (small,
  truly leaf). *Then* extract the `Error`/`StorageError`/`WeztermError`
  enums + their impls into `frankenterm-core-error-types`, with the
  classification crate as a sub-leaf dep. Two-step extract instead of one.
  Still satisfies mcp's needs.
- **(B) Move `error_kind` to a free function.** Refactor `Error::error_kind`
  → `network_reliability::classify_error(&err)`. Update all call sites
  (probably <20). Then `Error` is leaf-clean. One-step extract, but a real
  call-site refactor is on the critical path before any cargo work.
- **(C) Extract only the leaf-clean subset.** Move
  `RemediationCommand`/`Remediation`/`*OperationSource`/`WatchdogWarningSource`/
  `Result<T>` (~250 LOC of type defs) into `frankenterm-core-error-types`.
  Ships a real leaf crate but **does not unblock mcp_error.rs** (which
  needs the heavy enums). Useful as a foothold; not a true unblocker.

Option (B) is probably the right call — `error_kind` is small,
mechanical, and the call-site rewrite is shorter than two-step
extraction plumbing. But it requires a code change beyond pure file
moves, which goes beyond the "git mv + sed + re-export" pattern that
ft-y0loj.1/.2/.3.1 established.

## Other candidates I checked

- **`cass.rs` (2453 LOC)** — 5 cross-cluster deps (`agent_provider`,
  `error::Remediation`, `policy::Redactor`, `runtime_compat::process`,
  `storage`, `suggestions`). Not leaf-clean.
- **`caut.rs` (1722 LOC)** — same 5 deps as cass. Not leaf-clean.
- **`config.rs` (~1600 LOC of type defs)** — depends on connector, mcp,
  workflow types. Not extractable as a single leaf without a connector
  prerequisite.
- **`policy.rs`** — type defs are entangled with rule-evaluation logic and
  with connector telemetry composition. Substantial split needed.

**There is no obvious sub-1000-LOC leaf-clean extraction available.** The
parent ADR's "easiest leaf first, then chain" was optimistic.

## What I recommend

1. **Pick (B) for `error.rs`** — refactor `Error::error_kind` into a free
   `network_reliability::classify_error(&Error)` function (and the same for
   `StorageError` / `WeztermError`). Update the <20 call sites. Verify
   workspace check. Commit. *Then* extract `frankenterm-core-error-types`
   as a clean leaf with no first-party deps.
2. **After (1) lands**, attempt `frankenterm-core-config-types` (ft-otfxs).
   This will hit its own entanglement with connector + mcp types — expect
   a similar mid-extraction discovery requiring a sub-leaf.
3. **Defer `policy-types`** (ft-0pykm) until at least error+config land —
   policy.rs imports both Config and connector telemetry, and a clean cut
   probably requires three or four prerequisite leaves landing first.

This is multi-session work, not a 60-minute ship. Each leaf extraction
likely needs its own discovery + small refactor + extract + verify cycle.

## Pause point

I claimed ft-g6sa8 (in_progress) but did not start the file moves because
of the size mismatch. Awaiting direction:

- **Option α:** Ship Option (C) under ft-g6sa8 — extract the leaf-clean
  ~250 LOC subset of error.rs as a foothold. Closes ft-g6sa8 but doesn't
  unblock mcp.
- **Option β:** Ship Option (B) under ft-g6sa8 — refactor `error_kind` to
  a free function first, then extract the full error-types leaf. Closes
  ft-g6sa8 *and* unblocks mcp_error consumption. ~2x the work of (C).
- **Option γ:** Re-open ft-g6sa8 estimate from "117 LOC" to "small refactor
  + ~1486 LOC extract" and pause until next session.

I'll wait for the call.
