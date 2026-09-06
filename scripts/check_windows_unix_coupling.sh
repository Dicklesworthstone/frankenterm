#!/usr/bin/env bash
# ft-51fde — [win-compat][P0] CI guard: block NEW unconditional Unix couplings.
#
# Part of epic ft-azsnz (make ft build on Windows without changing Mac/Linux
# behavior). The Windows port is ADDITIVE ONLY: every Unix-specific surface must
# live behind a `#[cfg(unix)]` (or equivalent `target_os` / `target_family`)
# gate, or behind a `[target.'cfg(unix)'.dependencies]` table for crate deps.
#
# A strict remote RCH `cargo check --all-targets --target x86_64-pc-windows-msvc`
# is the authoritative development gate — it fails on an ungated coupling in
# Windows-reachable code. THIS script is a fast, build-free pre-flight that:
#   1. catches the common mistake (a new top-level `use std::os::unix` /
#      `std::os::fd`, or a new unix-only crate dependency) without waiting for a
#      full Windows toolchain build, and
#   2. covers surfaces `cargo check` does not typecheck on Windows (e.g.
#      `#[cfg(test)]` code), where an ungated coupling would silently rot until a
#      Windows test run.
#
# RATCHET SEMANTICS
# -----------------
# This is a one-way ratchet, not an absolute ban. Lots of legitimately *gated*
# Unix code already exists (the literal-move target state). To avoid flagging it,
# the current accepted set of ungated occurrences is recorded as a content-keyed
# BASELINE (scripts/windows-unix-coupling-baseline.txt). The guard fails only when
# a NEW ungated occurrence appears that is not already in the baseline.
#
#   * Keys are `path<TAB>trimmed-source-line` — NOT line numbers — so relocating
#     existing code (the "literal MOVE" the epic mandates) does not trip the guard.
#   * Removing a coupling is always allowed (the ratchet only tightens).
#   * To intentionally accept a new entry (e.g. you added a *gated* block that the
#     bounded-lookback heuristic can't see), re-bless the baseline in the same
#     commit:  BLESS=1 scripts/check_windows_unix_coupling.sh
#     and explain why in the commit message.
#
# UNIX IMPL IS UNCHANGED: this guard adds zero behavior; it only constrains future
# diffs. It never edits source.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

BASELINE="scripts/windows-unix-coupling-baseline.txt"

# ── Source-path couplings ────────────────────────────────────────────────────
# Unix-only std paths that do not compile on Windows.
SRC_PATTERN='std::os::(unix|fd)'

# Crate dependencies that are Unix-only (no Windows build). libc/rustix are
# intentionally EXCLUDED — both build on windows-msvc — as are mio/tokio etc.
# Keep this list to crates that genuinely lack a Windows target.
UNIX_ONLY_DEPS='nix|signal-hook|signal-hook-registry|signal-hook-mio|signal-hook-tokio|caps|uzers|daemonize|privdrop|termios|openpty|tty|sendfd|passfd'

# Emit, on stdout, every CURRENT ungated coupling as `path<TAB>key`.
# A source occurrence is "gated" when a `#[cfg(...)]` attribute mentioning
# unix / target_os / target_family appears within a bounded lookback window of
# non-blank lines above it (covers `#[cfg(unix)]\nuse ...`, `#[cfg(unix)] { use
# ... }`, and small gated fns). Anything outside that window is reported and, on
# first sight, absorbed into the baseline — so heuristic misses never block, but
# brand-new top-level couplings do.
collect_current() {
  # --- source paths ---
  # Tracked .rs files only; never touch target/ or vendored checkouts.
  git ls-files -- '*.rs' \
    | grep -vE '^(target/|.*/target/)' \
    | while IFS= read -r f; do
        awk -v file="$f" '
          # Track a sliding window of recent lines for gate detection.
          {
            line = $0
            # Detect a unix-ish cfg attribute on this line.
            is_cfg = (line ~ /#\[ *cfg(_attr)? *\(/) &&
                     (line ~ /unix/ || line ~ /target_os/ || line ~ /target_family/)
            # cfg_if! macro arms also gate; treat the macro line as a gate marker.
            if (line ~ /cfg_if *!/ || line ~ /cfg_if::cfg_if/) is_cfg = 1
          }
          # Does the target pattern appear (and not in a line comment)?
          {
            stripped = line
            sub(/\/\/.*/, "", stripped)   # drop trailing line comments for the hit test
          }
          stripped ~ /std::os::(unix|fd)/ {
            gated = (line ~ /#\[ *cfg/ && (line ~ /unix/ || line ~ /target_os/ || line ~ /target_family/))
            # look back through the recent window
            for (i = 1; i <= win_n; i++) {
              if (win_cfg[i]) { gated = 1; break }
            }
            if (!gated) {
              t = line
              sub(/^[ \t]+/, "", t); sub(/[ \t]+$/, "", t)
              printf "%s\t%s\n", file, t
            }
          }
          # Update window AFTER processing the line: window holds the N lines
          # ABOVE the next line. Reset window on a blank line or a closing brace
          # at column 0-ish (heuristic block boundary) to bound gate scope.
          {
            # shift
            for (i = win_max; i > 1; i--) { win_cfg[i] = win_cfg[i-1] }
            win_cfg[1] = is_cfg
            if (win_n < win_max) win_n++
            # Bound scope: a blank line ends a small gated region.
            if (line ~ /^[ \t]*$/) { for (i=1;i<=win_max;i++) win_cfg[i]=0; win_n=0 }
          }
          BEGIN { win_max = 8; win_n = 0 }
        ' "$f"
      done

  # --- unix-only crate deps in Cargo.toml ---
  # Flag a unix-only dep ONLY when it is declared outside a
  # `[target.'cfg(...unix...)'...]` table (i.e. unconditionally).
  git ls-files -- '*Cargo.toml' \
    | grep -vE '^(target/|.*/target/)' \
    | while IFS= read -r f; do
        awk -v file="$f" -v deps="${UNIX_ONLY_DEPS}" '
          BEGIN { split("", d); n=split(deps, arr, "|"); for(i=1;i<=n;i++) d[arr[i]]=1; in_unix_target=0 }
          # Section headers.
          /^[ \t]*\[/ {
            hdr=$0
            # A target table gated on unix is allowed.
            if (hdr ~ /\[ *target\./ && hdr ~ /cfg/ && hdr ~ /unix/) { in_unix_target=1 }
            else if (hdr ~ /\[ *target\./) { in_unix_target=0 }   # some OTHER target table (e.g. windows) — deps there are not unconditional-unix either
            else { in_unix_target=0 }                              # ordinary [dependencies] etc.
            next
          }
          {
            line=$0
            sub(/#.*/, "", line)                                   # strip comments
            # dep line form:  name = ... or name.workspace = true, possibly with quotes
            if (match(line, /^[ \t]*"?[A-Za-z0-9_-]+"?[ \t]*[=.]/)) {
              name=line
              sub(/^[ \t]*"?/, "", name); sub(/"?[ \t]*[=.].*$/, "", name)
              if ((name in d) && !in_unix_target) {
                t=$0; sub(/^[ \t]+/, "", t); sub(/[ \t]+$/, "", t)
                printf "%s\tDEP %s\n", file, t
              }
            }
          }
        ' "$f"
      done
}

CURRENT="$(collect_current | LC_ALL=C sort -u)"

# ── Bless mode: rewrite the baseline ─────────────────────────────────────────
if [[ "${BLESS:-0}" == "1" || "${1:-}" == "--update-baseline" ]]; then
  {
    echo "# ft-51fde windows unix-coupling baseline — content-keyed ratchet."
    echo "# Regenerate with: BLESS=1 scripts/check_windows_unix_coupling.sh"
    echo "# Each line: <path>\\t<trimmed source line>. Removing entries is always safe;"
    echo "# adding entries means you accepted a NEW ungated coupling — justify it in the commit."
    printf '%s\n' "${CURRENT}"
  } > "${BASELINE}"
  echo "ft-51fde guard: baseline written to ${BASELINE} ($(printf '%s\n' "${CURRENT}" | grep -c . || true) entries)."
  exit 0
fi

if [[ ! -f "${BASELINE}" ]]; then
  echo "ft-51fde guard: missing baseline ${BASELINE}. Generate it once with: BLESS=1 $0" >&2
  exit 2
fi

# Accepted set = non-comment, non-blank baseline lines.
ACCEPTED="$(grep -vE '^[[:space:]]*#' "${BASELINE}" | grep -vE '^[[:space:]]*$' | LC_ALL=C sort -u || true)"

# New violations = current entries not present in the accepted baseline.
NEW="$(LC_ALL=C comm -23 <(printf '%s\n' "${CURRENT}" | grep -vE '^[[:space:]]*$' || true) \
                          <(printf '%s\n' "${ACCEPTED}") || true)"

if [[ -z "${NEW}" ]]; then
  n=$(printf '%s\n' "${CURRENT}" | grep -c . || true)
  echo "ft-51fde guard: no NEW unconditional Unix couplings (${n} known-gated/baselined occurrences). clean."
  exit 0
fi

cat >&2 <<EOF
─── ft-51fde guard: NEW unconditional Unix coupling detected ───

Epic ft-azsnz requires the Windows port to be ADDITIVE and Unix-gated. The
following coupling(s) have no gate recognized by the bounded source lookback or
[target.'cfg(unix)'.dependencies] table check, and are not in the
accepted baseline (${BASELINE}):

${NEW}

What to do:
  • Source path (std::os::unix / std::os::fd):
      Gate it. Put the Unix-only item behind #[cfg(unix)] and add the Windows
      counterpart behind #[cfg(windows)] (use sysinfo / the windows crate safe
      surfaces / named-pipe libs — zero unsafe per ft-f6oi0). Do NOT change the
      Unix code path; move it verbatim behind the gate.

  • Unix-only crate dependency (nix, signal-hook, …):
      Move it under  [target.'cfg(unix)'.dependencies]  in the crate's Cargo.toml
      and provide a #[cfg(windows)] alternative, OR pick a cross-platform crate.

  • If this IS properly gated and the bounded-lookback heuristic just can't see
    the gate (e.g. a large gated fn), re-bless the baseline IN THE SAME COMMIT and
    say why:  BLESS=1 $0

The authoritative development check runs on an admissible Windows RCH worker:
  RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 RCH_WORKER=wsurf \\
    rch --no-self-healing exec -- cargo check -j 2 -p <crate> \\
    --all-targets --target x86_64-pc-windows-msvc --locked
Native release builds and release verification use DSR exclusively.
EOF
exit 1
