# Substrate Audit Checklist

**Session origin**: 24 audit passes across previously-unreviewed substrates surfaced
56 distinct bugs/gaps with a 100% hit rate on the patterns below. Closing
[`ft-i7hk4`](.beads/issues.jsonl) ships this checklist as the per-substrate
audit pattern future agents apply.

## How to use this checklist

When auditing a new substrate (`crates/frankenterm-core/src/<module>.rs`):

1. Walk the file top-to-bottom.
2. For each `pub struct` / `pub enum` / `pub fn`, check it against every
   pattern below.
3. File a bead per finding before fixing — `br create --type bug --priority p2
   --title "[audit] <module>.rs: <one-line>" --description "..."`.
4. Apply the standard fix template (see each pattern's "Fix" section).
5. Add a regression test pinning the new contract.
6. Commit + close.

Hit rate this session: 100% on previously-unreviewed substrates. ~19 substrates
fixed; estimated 10-30 more lurking in unreviewed modules.

---

## Pattern 1: pub-field state-machine bypass

**Smell**: A `pub struct` whose constructor (`new()`) enforces invariants
(clamps, normalises, validates), but whose fields are `pub` so external code
can write directly past the constructor.

**Threat model**: Same-process forgery. An attacker (or a confused integration)
mutates `signals.warning_threshold_ms = u64::MAX` — bypassing the constructor's
clamp — and the next decision function consumes the forged value.

**Examples found this session**: `ResizeDegradationSignals` (ft-mnua0),
`CircuitBreakerConfig` (ft-l5z7z), `ChunkBytes`, `WatchdogStats`,
`WatchdogConfig`, `CiMatrix.cells` (ft-vesy5 — release-gate forgery),
`SanitizationOutcome.sanitised_text` (ft-jqkw4 — sanitization bypass),
`ChunkMetadata.redaction` enum bypass.

**Fix**:

1. `pub` → `pub(crate)` on the fields.
2. Add public read accessors (`pub const fn field(&self) -> T`).
3. Add a public builder/with_* API that re-applies the invariant clamps.
4. Optionally add `with_repaired_invariants()` for adversarial-input repair
   at decision-function entry points.

**Defense-in-depth note**: For substrates whose fields are read by callers in
the same crate (or whose only consumers are integration tests), `pub(crate)`
with public accessors gives test-time flexibility while blocking same-process
forgery from external crates. The strongest defense is a content MAC at the
integration layer (see `audit_erasure_spec.rs` ft-h1hvw closing comment).

---

## Pattern 2: subprocess-argv flag injection

**Smell**: A function that builds `Command` / `argv` arguments using
user-controlled strings as positionals **without** the `--` end-of-options
sentinel.

**Threat model**: A user-supplied value that starts with `-` is interpreted as
a flag by the spawned binary instead of as positional data. Particularly
dangerous when the spawned binary (`grep`, `git`, `cass`) has flags that
trigger code execution (`--exec`, `--ext-cmd`, `--use-mailmap`, etc.).

**Example found this session**: `cass.rs::build_search_args` /
`build_query_args` (ft-vesy5 sweep) — user-controlled query strings flowed
positionally without `--`.

**Fix**:

1. Emit all flags first.
2. Insert `"--"` as a literal argument.
3. Append every user-controlled positional **after** the sentinel.

```rust
// BAD
cmd.arg("--option").arg(user_query);

// GOOD
cmd.arg("--option").arg("--").arg(user_query);
```

4. Test: pin a regression that passes `-x` as the user value and asserts the
   spawned binary sees it as positional, not as a flag.

**Audit scope for follow-up**: any module that calls `Command::new` /
`tokio::process::Command::new` with user-controlled inputs. Likely surfaces:
`workflows/runner.rs` (any subprocess paths), `policy.rs` (exec points),
`connector_*.rs`.

---

## Pattern 3: missing DoS caps on validated structs

**Smell**: A `pub struct` with collection fields (`Vec<T>`, `HashMap<K, V>`)
or variable-length string fields (`String`, `Vec<u8>`) and a constructor that
validates *content* but not *count* / *length*.

**Threat model**: An attacker or misconfigured operator sends a 1 GB tag,
1 million metadata entries, etc. Substrate accepts, downstream allocates,
process OOMs.

**Example found this session**: `agent_profiles.rs::ProfileValidation`
(ft-a2bt5) — validated tag/env/metadata content but had no count caps. Added
9 new caps + 8 new validation error variants.

**Fix**:

1. Define explicit `MAX_*` consts at module top:
   ```rust
   pub const TAGS_MAX_COUNT: usize = 32;
   pub const ENV_MAX_COUNT: usize = 64;
   pub const METADATA_KEY_MAX_LEN: usize = 256;
   ```
2. Wire into `validate()` with named error variants:
   ```rust
   if profile.tags.len() > TAGS_MAX_COUNT {
       return Err(ProfileValidationError::TooManyTags { ... });
   }
   ```
3. Test: pin one regression at `MAX + 1` per cap.

**Audit scope for follow-up**: every validated config struct in
`config_*.rs`, `agent_*.rs`, `connector_*.rs`. Look for `Vec<*>` and `String`
fields without `*_MAX_LEN` / `*_MAX_COUNT` constants in the same module.

---

## Pattern 4: attestation / release-gate vacuous-pass

**Smell**: A predicate `meets_*_bar()` or `is_*()` that returns `true` when
the underlying state is *empty* — the predicate is meant to gate releases on
"every required check passed" but happens to pass when "no checks were
recorded yet".

**Threat model**: A release attestation is generated from a fresh-state
snapshot (no real test runs), the predicate vacuously returns true, the
release ships without ever running the actual safety checks.

**Examples found this session**: `CellCrcStats::is_clean` (ft-vqohn) —
returned true at cold start because mismatch counters were zero;
`CiMatrix::meets_release_bar` (ft-vesy5) — passed for empty matrices;
`RedrawDecisionHealth::is_safe` (ft-yxrez) — vacuous-passed even when
`force_paint_counters` had observability data without predicate evaluations.

**Fix**:

1. Pair the predicate with a coverage check (e.g.,
   `covers_full_matrix && meets_each_cell`).
2. Require ≥1 of the relevant counter to be non-zero before vacuous-pass:
   ```rust
   pub fn is_clean(&self) -> bool {
       self.frames_hashed_total > 0
           && self.both_differ_total == 0
           && self.only_fnv_differs_total == 0
           && self.only_crc_differs_total == 0
   }
   ```
3. Test: pin a regression that asserts the cold default returns `false`.

**Audit scope for follow-up**: every `meets_*` / `is_*` / `*_passes` /
`*_complete` predicate in the codebase. Especially audit attestation builders
(`crates/frankenterm-core/src/*_attestation*.rs`).

---

## Pattern 5: redactor pattern coverage drift

**Smell**: The redactor's pattern catalog falls behind the threat
landscape — new cloud / OAuth / API providers ship credentials with
recognisable prefixes, and an unaudited redactor leaks them.

**Examples found this session** (ft-8nd26): added 5 patterns —
`JWT_TOKEN`, `GITLAB_TOKEN`, `TWILIO_ACCOUNT_SID`, `SENDGRID_KEY`,
`DATADOG_API_KEY`. Highest-impact gap was JWT (every `eyJ...` token in
output).

**Fix**:

1. Quarterly cadence: review provider docs for new credential prefixes.
2. Add patterns to `crates/frankenterm-core/src/redactor.rs` `PATTERNS`
   array.
3. Add tests in the same file's `tests::` covering positive +
   adjacent-context negative cases.

**Audit scope for follow-up next cycle**: Azure SAS tokens, Cloudflare API
keys, Discord webhook tokens, JWT-with-other-prefixes (different `eyJ`
shapes), AWS session tokens (`ASIA*`).

---

## Bead-filing template

```sh
br create --type bug --priority p2 \
  --title "[audit] <module>.rs: <one-line>" \
  --description "$(cat <<'EOF'
Audit findings in crates/frankenterm-core/src/<module>.rs:

(A) <pattern>: <specific defect>
    Threat: <repro path>
    Fix: <pattern's standard fix template>

(B) <next defect> ...
EOF
)"
```

When closing: include the substrate scope, regression test count, and any
deferred work.
