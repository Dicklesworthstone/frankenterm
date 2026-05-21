#!/usr/bin/env bash
#
# FrankenTerm (ft) installer
#
# One-liner install (with cache buster):
#   curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/frankenterm/main/install.sh?$(date +%s)" | bash
#
# Or without cache buster:
#   curl -fsSL https://raw.githubusercontent.com/Dicklesworthstone/frankenterm/main/install.sh | bash
#
# Options:
#   --version vX.Y.Z   Install specific version (default: latest)
#   --dest DIR         Install to DIR (default: ~/.local/bin)
#   --system           Install to /usr/local/bin (requires sudo)
#   --easy-mode        Auto-update PATH in shell rc files
#   --verify           Run `ft doctor` after install
#   --with-font        Also install the bundled Pragmasevka Nerd Font
#   --from-source      Build from source instead of downloading binary
#   --quiet            Suppress non-error output
#   --no-gum           Disable gum formatting even if available
#   --no-verify        Skip checksum + signature verification (for testing only)
#   --offline TARBALL  Skip network entirely; install from local tarball
#   --force            Force reinstall even if same version is installed
#   --help             Show this message
#
# Environment overrides:
#   VERSION, OWNER, REPO, DEST, ARTIFACT_URL, CHECKSUM, CHECKSUM_URL,
#   SIGSTORE_BUNDLE_URL, COSIGN_IDENTITY_RE, COSIGN_OIDC_ISSUER,
#   HTTP_PROXY, HTTPS_PROXY
#
set -euo pipefail
umask 022
shopt -s lastpipe 2>/dev/null || true

# Reject non-bash interpreters AND bash-in-POSIX-mode. We use bashisms
# throughout (arrays, [[ ]], +=, C-style for, ${arr[@]+…} idiom, echo -e,
# etc.); dash / ash / busybox sh / zsh would crash with cryptic syntax
# errors many lines deep. On macOS `/bin/sh` is bash in POSIX mode
# (BASH_VERSION still set, but `echo -e` and other bashisms disabled),
# so checking BASH_VERSION alone isn't enough — POSIXLY_CORRECT
# detects the POSIX-mode case.
if [ -z "${BASH_VERSION:-}" ]; then
  echo "Error: this installer requires bash (not sh/dash/zsh)." >&2
  echo "Re-run with: bash install.sh   (or pipe to: ... | bash)" >&2
  exit 1
fi
if [ -n "${POSIXLY_CORRECT:-}" ]; then
  echo "Error: this installer requires bash in non-POSIX mode." >&2
  echo "You appear to be running bash via /bin/sh or with --posix set." >&2
  echo "Re-run with: bash install.sh   (or pipe to: ... | bash)" >&2
  exit 1
fi

VERSION="${VERSION:-}"
OWNER="${OWNER:-Dicklesworthstone}"
REPO="${REPO:-frankenterm}"
DEST_DEFAULT="$HOME/.local/bin"
DEST="${DEST:-$DEST_DEFAULT}"
EASY=0
QUIET=0
VERIFY=0
WITH_FONT=0
FROM_SOURCE=0
NO_GUM=0
NO_CHECKSUM=0
FORCE_INSTALL=0
OFFLINE_TARBALL=""
CHECKSUM="${CHECKSUM:-}"
CHECKSUM_URL="${CHECKSUM_URL:-}"
SIGSTORE_BUNDLE_URL="${SIGSTORE_BUNDLE_URL:-}"
COSIGN_IDENTITY_RE="${COSIGN_IDENTITY_RE:-^https://github.com/${OWNER}/${REPO}/.github/workflows/release.yml@refs/tags/.*$}"
COSIGN_OIDC_ISSUER="${COSIGN_OIDC_ISSUER:-https://token.actions.githubusercontent.com}"
ARTIFACT_URL="${ARTIFACT_URL:-}"
LOCK_FILE="/tmp/ft-install.lock"
HARDCODED_FALLBACK_VERSION="v0.2.0"

# Cleanup state. Initialised to safe defaults so the EXIT trap (registered
# *before* lock acquisition) can run even if we abort partway through init —
# e.g., between `mkdir $LOCK_DIR` and `mktemp -d`. Without this, an mktemp
# failure on a held lock would leak the lock dir permanently.
TMP=""
LOCK_DIR=""
LOCKED=0
cleanup() {
  [ -n "$TMP" ] && rm -rf "$TMP"
  [ "$LOCKED" -eq 1 ] && [ -n "$LOCK_DIR" ] && rm -rf "$LOCK_DIR"
}
trap cleanup EXIT

# Proxy support — populated by setup_proxy(), passed to every curl call.
# We use the `${arr[@]+"${arr[@]}"}` idiom (rather than the simpler
# `"${arr[@]}"`) at every call site because macOS still ships bash 3.2
# as /bin/bash, and bash 3.2 + `set -u` treats `"${arr[@]}"` on an
# empty array as "unbound variable". curl|bash users on a stock macOS
# without Homebrew bash would otherwise crash on every empty-proxy
# expansion. Bash 4.4+ handles the simple form, but we need 3.2 compat.
PROXY_ARGS=()

# Detect gum for fancy output (https://github.com/charmbracelet/gum)
HAS_GUM=0
if command -v gum &> /dev/null && [ -t 1 ]; then
  HAS_GUM=1
fi

# ───────────────────────────────────────────────────────────────────────────
# Logging helpers (gum + ANSI fallback)
# ───────────────────────────────────────────────────────────────────────────
info() {
  [ "$QUIET" -eq 1 ] && return 0
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    gum style --foreground 39 "→ $*"
  else
    echo -e "\033[0;34m→\033[0m $*"
  fi
}

ok() {
  [ "$QUIET" -eq 1 ] && return 0
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    gum style --foreground 42 "✓ $*"
  else
    echo -e "\033[0;32m✓\033[0m $*"
  fi
}

warn() {
  [ "$QUIET" -eq 1 ] && return 0
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    gum style --foreground 214 "⚠ $*"
  else
    echo -e "\033[1;33m⚠\033[0m $*"
  fi
}

err() {
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    gum style --foreground 196 "✗ $*"
  else
    echo -e "\033[0;31m✗\033[0m $*"
  fi
}

run_with_spinner() {
  local title="$1"
  shift
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ] && [ "$QUIET" -eq 0 ]; then
    gum spin --spinner dot --title "$title" -- "$@"
  else
    info "$title"
    "$@"
  fi
}

draw_box() {
  local color="$1"; shift
  local lines=("$@")
  local max_width=0
  local esc; esc=$(printf '\033')
  local strip_ansi_sed="s/${esc}\\[[0-9;]*m//g"

  for line in ${lines[@]+"${lines[@]}"}; do
    local stripped
    stripped=$(printf '%b' "$line" | LC_ALL=C sed "$strip_ansi_sed")
    local len=${#stripped}
    [ "$len" -gt "$max_width" ] && max_width=$len
  done

  local inner_width=$((max_width + 4))
  local border=""
  for ((i=0; i<inner_width; i++)); do border+="═"; done

  printf "\033[%sm╔%s╗\033[0m\n" "$color" "$border"
  for line in ${lines[@]+"${lines[@]}"}; do
    local stripped
    stripped=$(printf '%b' "$line" | LC_ALL=C sed "$strip_ansi_sed")
    local len=${#stripped}
    local padding=$((max_width - len))
    local pad_str=""
    for ((i=0; i<padding; i++)); do pad_str+=" "; done
    printf "\033[%sm║\033[0m  %b%s  \033[%sm║\033[0m\n" "$color" "$line" "$pad_str" "$color"
  done
  printf "\033[%sm╚%s╝\033[0m\n" "$color" "$border"
}

# ───────────────────────────────────────────────────────────────────────────
# Proxy + platform + version detection
# ───────────────────────────────────────────────────────────────────────────
setup_proxy() {
  PROXY_ARGS=()
  if [[ -n "${HTTPS_PROXY:-}" ]]; then
    PROXY_ARGS=(--proxy "$HTTPS_PROXY")
  elif [[ -n "${HTTP_PROXY:-}" ]]; then
    PROXY_ARGS=(--proxy "$HTTP_PROXY")
  fi
}

detect_platform() {
  OS=$(uname -s | tr '[:upper:]' '[:lower:]')
  ARCH=$(uname -m)
  case "$ARCH" in
    x86_64|amd64) ARCH="x86_64" ;;
    arm64|aarch64) ARCH="aarch64" ;;
    *) warn "Unknown arch $ARCH, using as-is" ;;
  esac

  # WSL warning (continue with linux platform)
  if [[ "$OS" == "linux" ]] && grep -qi microsoft /proc/version 2>/dev/null; then
    warn "WSL detected. The FrankenTerm GUI is macOS-only for now;"
    warn "the ft CLI + frankenterm-mux-server work fine under WSL."
  fi

  # FrankenTerm release assets are named ft-{os}-{arch}.tar.xz where:
  #   arch ∈ {amd64, arm64}   — NOT Rust triples
  #   os   ∈ {linux, darwin}
  # See .github/workflows/release.yml asset names.
  ASSET=""
  TARGET="" # informational only — matches release.yml matrix target
  case "${OS}-${ARCH}" in
    linux-x86_64)    ASSET="ft-linux-amd64.tar.xz";  TARGET="x86_64-unknown-linux-gnu"  ;;
    linux-aarch64)   ASSET="ft-linux-arm64.tar.xz";  TARGET="aarch64-unknown-linux-gnu" ;;
    darwin-aarch64)  ASSET="ft-darwin-arm64.tar.xz"; TARGET="aarch64-apple-darwin"      ;;
    darwin-x86_64)
      warn "Intel Mac (x86_64-apple-darwin) is not in the v0.2.0 release matrix:"
      warn "ONNX Runtime (semantic-search default feature) has no Intel-Mac prebuilts."
      warn "Falling back to build-from-source. Pass --from-source to skip this notice."
      FROM_SOURCE=1
      TARGET="x86_64-apple-darwin"
      ;;
    *)
      warn "No prebuilt artifact for ${OS}/${ARCH}; falling back to build-from-source"
      FROM_SOURCE=1
      ;;
  esac
}

set_artifact_url() {
  TAR=""
  URL=""
  if [ "$FROM_SOURCE" -eq 0 ] && [ -z "$OFFLINE_TARBALL" ]; then
    if [ -n "$ARTIFACT_URL" ]; then
      TAR=$(basename "$ARTIFACT_URL")
      URL="$ARTIFACT_URL"
    elif [ -n "$ASSET" ]; then
      TAR="$ASSET"
      URL="https://github.com/${OWNER}/${REPO}/releases/download/${VERSION}/${TAR}"
    fi
  fi
}

resolve_version() {
  if [ -n "$VERSION" ]; then
    info "Using requested version: $VERSION"
    return 0
  fi
  # Primary: GitHub API
  if command -v curl >/dev/null 2>&1; then
    local api_url="https://api.github.com/repos/${OWNER}/${REPO}/releases/latest"
    local resolved
    resolved=$(curl -fsSL --max-time 10 ${PROXY_ARGS[@]+"${PROXY_ARGS[@]}"} "$api_url" 2>/dev/null \
      | grep '"tag_name":' \
      | sed -E 's/.*"([^"]+)".*/\1/' \
      | head -1 || true)
    if [ -n "$resolved" ]; then
      VERSION="$resolved"
      info "Resolved latest version: $VERSION"
      return 0
    fi
    # Fallback: parse redirect URL of /releases/latest
    resolved=$(curl -fsSL --max-time 10 ${PROXY_ARGS[@]+"${PROXY_ARGS[@]}"} \
      -o /dev/null -w '%{url_effective}' \
      "https://github.com/${OWNER}/${REPO}/releases/latest" 2>/dev/null \
      | sed -E 's|.*/tag/||' || true)
    if [ -n "$resolved" ] && [[ "$resolved" == v* ]]; then
      VERSION="$resolved"
      info "Resolved latest version (via redirect): $VERSION"
      return 0
    fi
  fi
  warn "Could not resolve latest version; using hardcoded fallback $HARDCODED_FALLBACK_VERSION"
  VERSION="$HARDCODED_FALLBACK_VERSION"
}

# ───────────────────────────────────────────────────────────────────────────
# Preflight
# ───────────────────────────────────────────────────────────────────────────
check_disk_space() {
  # 50MB headroom. The ft release binary is ~19MB on macOS arm64 / ~15MB on
  # Linux; tarball download + uncompressed extract + final installed copy
  # all coexist briefly under $TMP and $DEST.
  local min_kb=51200
  local path="$DEST"
  [ ! -d "$path" ] && path=$(dirname "$path")
  if command -v df >/dev/null 2>&1; then
    local avail_kb
    avail_kb=$(df -Pk "$path" 2>/dev/null | awk 'NR==2 {print $4}')
    if [ -n "$avail_kb" ] && [ "$avail_kb" -lt "$min_kb" ]; then
      err "Insufficient disk space in $path (need at least 50MB)"
      exit 1
    fi
  else
    warn "df not found; skipping disk space check"
  fi
}

check_write_permissions() {
  if [ ! -d "$DEST" ]; then
    if ! mkdir -p "$DEST" 2>/dev/null; then
      err "Cannot create $DEST (insufficient permissions)"
      err "Try --system (with sudo) or pick a writable --dest"
      exit 1
    fi
  fi
  if [ ! -w "$DEST" ]; then
    err "No write permission to $DEST"
    err "Try --system (with sudo) or pick a writable --dest"
    exit 1
  fi
}

check_existing_install() {
  if [ -x "$DEST/ft" ]; then
    local current
    current=$("$DEST/ft" --version 2>/dev/null | head -1 || echo "")
    [ -n "$current" ] && info "Existing ft detected: $current"
  fi
}

check_network() {
  [ -n "$OFFLINE_TARBALL" ] && { info "Offline mode (--offline); skipping network preflight"; return 0; }
  [ "$FROM_SOURCE" -eq 1 ] && return 0
  [ -z "$URL" ] && return 0
  command -v curl >/dev/null 2>&1 || { warn "curl not found; skipping network check"; return 0; }
  # 10s total cap — generous enough to absorb a slow first byte on
  # high-latency links (LTE, satellite, throttled corp proxies) without
  # firing the false-positive warn that the previous 5s budget produced.
  if ! curl -fsSL ${PROXY_ARGS[@]+"${PROXY_ARGS[@]}"} --connect-timeout 5 --max-time 10 -o /dev/null -I "$URL"; then
    warn "Network check failed for $URL"
    warn "Continuing; download may fail"
  fi
}

preflight_checks() {
  info "Running preflight checks"
  check_disk_space
  check_write_permissions
  check_existing_install
  check_network
}

check_installed_version() {
  local target_ver="$1"
  [ -x "$DEST/ft" ] || return 1
  local cur
  cur=$("$DEST/ft" --version 2>/dev/null | head -1 | awk '{print $2}' || echo "")
  [ -z "$cur" ] && return 1
  # Strip leading 'v' from target_ver for comparison ("v0.2.0" vs "0.2.0")
  local stripped="${target_ver#v}"
  [ "$cur" = "$stripped" ] || [ "$cur" = "$target_ver" ]
}

# ───────────────────────────────────────────────────────────────────────────
# PATH integration
# ───────────────────────────────────────────────────────────────────────────
maybe_add_path() {
  case ":$PATH:" in
    *:"$DEST":*) return 0 ;;
    *)
      if [ "$EASY" -eq 1 ]; then
        local updated=0
        for rc in "$HOME/.zshrc" "$HOME/.bashrc"; do
          if [ -e "$rc" ] && [ -w "$rc" ]; then
            if ! grep -F "$DEST" "$rc" >/dev/null 2>&1; then
              # Leading newline ensures we don't accidentally append to a
              # line that didn't end with one (rc files don't always have
              # a trailing newline). printf gives precise control.
              # shellcheck disable=SC2016
              # ^ The literal `$PATH` is intentional — it must stay as a
              #   shell-variable reference to be expanded at the user's
              #   shell startup, not interpolated here at install time.
              printf '\n# Added by FrankenTerm installer\nexport PATH="%s:$PATH"\n' \
                "$DEST" >> "$rc"
            fi
            updated=1
          fi
        done
        if [ "$updated" -eq 1 ]; then
          warn "PATH updated in ~/.zshrc/.bashrc; restart shell to use ft"
        else
          warn "Add $DEST to PATH to use ft"
        fi
      else
        warn "Add $DEST to PATH to use ft (or rerun with --easy-mode)"
      fi
      ;;
  esac
}

# ───────────────────────────────────────────────────────────────────────────
# Checksum + Sigstore verification
# ───────────────────────────────────────────────────────────────────────────
verify_checksum() {
  local file="$1"; local expected="$2"; local actual=""
  if [ ! -f "$file" ]; then err "File not found: $file"; return 1; fi
  if command -v sha256sum &>/dev/null; then
    actual=$(sha256sum "$file" | awk '{print $1}')
  elif command -v shasum &>/dev/null; then
    actual=$(shasum -a 256 "$file" | awk '{print $1}')
  else
    warn "No sha256sum or shasum found; skipping checksum verification"
    return 0
  fi
  if [ "$actual" != "$expected" ]; then
    err "Checksum verification FAILED"
    err "Expected: $expected"
    err "Got:      $actual"
    err "The downloaded file may be corrupted or tampered with."
    rm -f "$file"
    return 1
  fi
  ok "Checksum verified: ${actual:0:16}..."
  return 0
}

verify_sigstore_bundle() {
  local file="$1"; local artifact_url="$2"
  if ! command -v cosign &>/dev/null; then
    warn "cosign not found; skipping signature verification"
    warn "Install cosign for stronger authenticity checks: https://docs.sigstore.dev/cosign/installation/"
    return 0
  fi
  # Per-binary sigstore bundles aren't published yet for FrankenTerm
  # (the v0.2.0 release ships only a bundle-level sigstore at
  # 0.2.0.sigstore which signs the attestation JSON, not each tarball).
  # If/when per-asset bundles ship, the URL convention will be
  # ${artifact_url}.sigstore.json — match that and verify; otherwise skip.
  local bundle_url="$SIGSTORE_BUNDLE_URL"
  [ -z "$bundle_url" ] && bundle_url="${artifact_url}.sigstore.json"
  local bundle_file
  bundle_file="$TMP/$(basename "$bundle_url")"
  if ! curl -fsSL --max-time 10 ${PROXY_ARGS[@]+"${PROXY_ARGS[@]}"} "$bundle_url" -o "$bundle_file" 2>/dev/null; then
    warn "Sigstore bundle not found at $bundle_url; skipping signature verification"
    return 0
  fi
  if ! cosign verify-blob \
      --bundle "$bundle_file" \
      --certificate-identity-regexp "$COSIGN_IDENTITY_RE" \
      --certificate-oidc-issuer "$COSIGN_OIDC_ISSUER" \
      "$file"; then
    return 1
  fi
  ok "Signature verified (cosign)"
  return 0
}

# ───────────────────────────────────────────────────────────────────────────
# Optional: Pragmasevka Nerd Font install
# ───────────────────────────────────────────────────────────────────────────
install_pragmasevka() {
  # --offline promises no network; honour that for the font too.
  if [ -n "$OFFLINE_TARBALL" ]; then
    warn "Skipping --with-font in --offline mode (no network)."
    warn "Install the font manually from your distro / Homebrew if needed."
    return 0
  fi
  # FrankenTerm bundles Pragmasevka NF v1.7.0 in its repo at
  # crates/frankenterm/assets/Pragmasevka_NF.zip.zst. Despite the
  # `.zip.zst` filename, the inner payload is a TAR archive (built
  # that way by scripts/create-macos-bundle.sh — see
  # `zstd -dc … | /usr/bin/tar -xf -` there). We use `tar -xf` after
  # zstd-decompression for parity.
  local font_url="https://raw.githubusercontent.com/${OWNER}/${REPO}/${VERSION}/crates/frankenterm/assets/Pragmasevka_NF.zip.zst"
  local font_dir=""
  case "$OS" in
    linux)  font_dir="${XDG_DATA_HOME:-$HOME/.local/share}/fonts/pragmasevka" ;;
    darwin) font_dir="$HOME/Library/Fonts/pragmasevka" ;;
    *)      warn "Unknown OS for font install; skipping"; return 0 ;;
  esac
  command -v zstd >/dev/null 2>&1 || { warn "zstd not found; skipping font install (install with: brew install zstd | apt install zstd)"; return 0; }
  command -v tar  >/dev/null 2>&1 || { warn "tar not found; skipping font install"; return 0; }
  # install_pragmasevka is best-effort — a mkdir failure (locked-down
  # system, ENOSPC, etc.) must not abort the whole installer. Wrap with
  # a graceful return so the user still gets a successful ft install.
  if ! mkdir -p "$font_dir" 2>/dev/null; then
    warn "Could not create font dir $font_dir; skipping font install"
    return 0
  fi
  info "Fetching Pragmasevka NF from $font_url"
  if ! curl -fsSL --max-time 60 ${PROXY_ARGS[@]+"${PROXY_ARGS[@]}"} "$font_url" -o "$TMP/pragmasevka.zip.zst"; then
    warn "Pragmasevka payload download failed; skipping font install"
    return 0
  fi
  if ! zstd -dc "$TMP/pragmasevka.zip.zst" | tar -xf - -C "$font_dir" 2>/dev/null; then
    warn "Pragmasevka payload extraction failed; skipping"
    return 0
  fi
  ok "Pragmasevka NF installed to $font_dir"
  if [ "$OS" = "linux" ] && command -v fc-cache >/dev/null 2>&1; then
    run_with_spinner "Refreshing font cache" fc-cache -f "$font_dir" || true
    ok "Font cache refreshed"
  fi
}

# ───────────────────────────────────────────────────────────────────────────
# Build-from-source fallback
# ───────────────────────────────────────────────────────────────────────────
ensure_rust() {
  if command -v cargo >/dev/null 2>&1; then return 0; fi
  warn "Rust toolchain not found; installing rustup"
  curl --proto '=https' --tlsv1.2 -fsSL ${PROXY_ARGS[@]+"${PROXY_ARGS[@]}"} https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal
  export PATH="$HOME/.cargo/bin:$PATH"
}

build_from_source() {
  info "Building ft from source — this takes 10-30+ minutes cold-cache"
  ensure_rust
  command -v git >/dev/null 2>&1 || { err "git is required for --from-source"; exit 1; }
  # Try the user-specified version first. If that fails (typo, tag doesn't
  # exist, branch was renamed), wipe any partial clone state and try the
  # default branch as a last-resort fallback. Without the explicit rm -rf
  # between attempts, the second clone fails too because $TMP/src may not
  # be empty.
  if ! git clone --depth 1 --branch "$VERSION" \
       "https://github.com/${OWNER}/${REPO}.git" "$TMP/src" 2>/dev/null; then
    rm -rf "$TMP/src"
    if ! git clone --depth 1 \
         "https://github.com/${OWNER}/${REPO}.git" "$TMP/src"; then
      err "Failed to clone ${OWNER}/${REPO} (tried --branch $VERSION then default)"
      err "Check network and that https://github.com/${OWNER}/${REPO} exists"
      exit 1
    fi
    warn "Tag/branch '$VERSION' not found; built from default branch instead"
  fi
  # Build only the ft CLI (not the GUI/mux-server) for the broadest
  # platform coverage. Users who want the macOS .app should install
  # from the .app bundle (separate flow) or build the workspace
  # directly: `cargo build --release` after cloning.
  # Friendly error wrapping: a bare `set -e` exit on cargo failure would
  # not give the user any actionable diagnosis.
  if ! ( cd "$TMP/src" && cargo build --release -p frankenterm --bin ft ); then
    err "Source build failed."
    err "Common causes:"
    err "  - Missing system deps on Linux: pkg-config, libcairo2-dev,"
    err "    libx11-dev, libx11-xcb-dev, libxcb-util-dev, libxcb-image0-dev,"
    err "    libxkbcommon-dev, libxkbcommon-x11-dev."
    err "  - Out-of-disk during compile (cargo's target/ uses 10+ GB)."
    err "  - Old Rust toolchain (FrankenTerm needs Rust 1.85+)."
    exit 1
  fi
  local bin="$TMP/src/target/release/ft"
  [ -x "$bin" ] || { err "Build did not produce $bin"; exit 1; }
  install -m 0755 "$bin" "$DEST/ft"
  ok "Installed to $DEST/ft (source build)"
}

# ───────────────────────────────────────────────────────────────────────────
# Usage + arg parsing
# ───────────────────────────────────────────────────────────────────────────
usage() {
  cat <<EOFU
Usage: install.sh [--version vX.Y.Z] [--dest DIR] [--system] [--easy-mode]
                  [--verify] [--with-font] [--from-source] [--quiet]
                  [--no-gum] [--no-verify] [--offline TARBALL] [--force]
                  [--artifact-url URL] [--checksum HEX] [--checksum-url URL]
                  [--help]

Options:
  --version vX.Y.Z   Install specific version (default: latest)
  --dest DIR         Install to DIR (default: ~/.local/bin)
  --system           Install to /usr/local/bin (requires sudo)
  --easy-mode        Auto-update PATH in shell rc files
  --verify           Run \`ft doctor\` after install
  --with-font        Also install the bundled Pragmasevka Nerd Font
  --from-source      Build from source (slow; requires Rust + git)
  --quiet, -q        Suppress non-error output
  --no-gum           Disable gum formatting even if available
  --no-verify        Skip checksum + signature verification (for testing only)
  --offline TARBALL  Install from local tarball; skip all network calls
  --force            Force reinstall even if same version is installed
  --artifact-url URL Override artifact URL (e.g. custom mirror)
  --checksum HEX     Inline SHA256 (skips checksum fetch)
  --checksum-url URL Override checksum file URL
  --help, -h         Show this message

Environment overrides:
  VERSION, OWNER, REPO, DEST, ARTIFACT_URL, CHECKSUM, CHECKSUM_URL,
  SIGSTORE_BUNDLE_URL, COSIGN_IDENTITY_RE, COSIGN_OIDC_ISSUER,
  HTTP_PROXY, HTTPS_PROXY
EOFU
}

require_option_value() {
  local option="$1"; local value="${2:-}"
  if [ -z "$value" ] || [[ "$value" == -* ]]; then
    err "$option requires a value"; usage; exit 2
  fi
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version) require_option_value "$1" "${2:-}"; VERSION="$2"; shift 2 ;;
    --dest) require_option_value "$1" "${2:-}"; DEST="$2"; shift 2 ;;
    --system) DEST="/usr/local/bin"; shift ;;
    --easy-mode) EASY=1; shift ;;
    --verify) VERIFY=1; shift ;;
    --with-font) WITH_FONT=1; shift ;;
    --from-source) FROM_SOURCE=1; shift ;;
    --quiet|-q) QUIET=1; shift ;;
    --no-gum) NO_GUM=1; shift ;;
    --no-verify) NO_CHECKSUM=1; shift ;;
    --offline) require_option_value "$1" "${2:-}"; OFFLINE_TARBALL="$2"; shift 2 ;;
    --force) FORCE_INSTALL=1; shift ;;
    --artifact-url) require_option_value "$1" "${2:-}"; ARTIFACT_URL="$2"; shift 2 ;;
    --checksum) require_option_value "$1" "${2:-}"; CHECKSUM="$2"; shift 2 ;;
    --checksum-url) require_option_value "$1" "${2:-}"; CHECKSUM_URL="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) warn "Unknown option: $1 (ignored)"; shift ;;
  esac
done

# ───────────────────────────────────────────────────────────────────────────
# Header banner
# ───────────────────────────────────────────────────────────────────────────
if [ "$QUIET" -eq 0 ]; then
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    gum style \
      --border normal \
      --border-foreground 39 \
      --padding "0 1" \
      --margin "1 0" \
      "$(gum style --foreground 42 --bold 'FrankenTerm installer')" \
      "$(gum style --foreground 245 'Swarm-native terminal platform for AI agent fleets')"
  else
    echo
    echo -e "\033[1;32mFrankenTerm installer\033[0m"
    echo -e "\033[0;90mSwarm-native terminal platform for AI agent fleets\033[0m"
    echo
  fi
fi

# ───────────────────────────────────────────────────────────────────────────
# Required tooling: curl + tar. We use both at multiple call sites and the
# failure mode of "command not found" mid-flow is opaque (set -e exit with
# no friendly message), so check upfront. Offline mode still needs tar to
# extract the local tarball, and curl is used by ensure_rust /
# install_pragmasevka even when the main download path is bypassed.
# ───────────────────────────────────────────────────────────────────────────
for required in curl tar; do
  if ! command -v "$required" >/dev/null 2>&1; then
    err "Required tool not found: $required"
    err "Install $required via your package manager:"
    err "  macOS:        $required is shipped by default; check your PATH"
    err "  Debian/Ubuntu: sudo apt-get install -y $required"
    err "  RHEL/Fedora:   sudo dnf install -y $required"
    err "  Alpine:        sudo apk add $required"
    exit 1
  fi
done

# ───────────────────────────────────────────────────────────────────────────
# Resolve, detect, preflight
# ───────────────────────────────────────────────────────────────────────────
setup_proxy
if [ -n "$OFFLINE_TARBALL" ]; then
  [ -f "$OFFLINE_TARBALL" ] || { err "Offline tarball not found: $OFFLINE_TARBALL"; exit 1; }
  # --offline takes precedence over --from-source: the user explicitly
  # supplied a binary tarball, so use that even if they also passed
  # --from-source (which detect_platform may also auto-set on Intel Mac
  # or unknown platforms). Without this override the offline tarball
  # would be cp'd then ignored when the FROM_SOURCE branch runs cargo.
  FROM_SOURCE=0
  # In offline mode we still need to know the platform/asset name for extraction
  detect_platform
  # detect_platform may have set FROM_SOURCE=1 for Intel Mac / unknown
  # platforms; offline tarball still wins. Clear it again post-detect.
  FROM_SOURCE=0
  TAR=$(basename "$OFFLINE_TARBALL")
  URL=""
else
  resolve_version
  detect_platform
  set_artifact_url
fi

mkdir -p "$DEST" 2>/dev/null || true
preflight_checks

# Already at target version short-circuit (unless --force or offline).
# Note: the EXIT trap is already registered at script init, so any TMP
# we create here is auto-cleaned regardless of how we exit.
if [ "$FORCE_INSTALL" -eq 0 ] && [ -z "$OFFLINE_TARBALL" ] && [ -n "$VERSION" ] \
    && check_installed_version "$VERSION"; then
  ok "ft $VERSION is already installed at $DEST/ft"
  info "Use --force to reinstall"
  if [ "$WITH_FONT" -eq 1 ]; then
    TMP=$(mktemp -d)
    install_pragmasevka
  fi
  exit 0
fi

# ───────────────────────────────────────────────────────────────────────────
# Atomic lock (mkdir-based — works on macOS without flock)
# ───────────────────────────────────────────────────────────────────────────
LOCK_DIR="${LOCK_FILE}.d"
if mkdir "$LOCK_DIR" 2>/dev/null; then
  LOCKED=1
  echo $$ > "$LOCK_DIR/pid"
else
  if [ -f "$LOCK_DIR/pid" ]; then
    OLD_PID=$(cat "$LOCK_DIR/pid" 2>/dev/null || echo "")
    if [ -n "$OLD_PID" ] && ! kill -0 "$OLD_PID" 2>/dev/null; then
      rm -rf "$LOCK_DIR"
      if mkdir "$LOCK_DIR" 2>/dev/null; then
        LOCKED=1; echo $$ > "$LOCK_DIR/pid"
      fi
    fi
  fi
  if [ "$LOCKED" -eq 0 ]; then
    err "Another installer is running (lock $LOCK_DIR)"
    err "If you're certain no installer is running (e.g., previous run was"
    err "SIGKILL'd between mkdir and PID write), remove the lock manually:"
    err "  rm -rf $LOCK_DIR"
    # Clear LOCK_DIR so the trap doesn't try to clean another installer's lock
    LOCK_DIR=""
    exit 1
  fi
fi

TMP=$(mktemp -d)

# ───────────────────────────────────────────────────────────────────────────
# Download / source build / offline-tarball selection
# ───────────────────────────────────────────────────────────────────────────
if [ -n "$OFFLINE_TARBALL" ]; then
  info "Using offline tarball: $OFFLINE_TARBALL"
  cp "$OFFLINE_TARBALL" "$TMP/$TAR"
elif [ "$FROM_SOURCE" -eq 0 ]; then
  if [ -z "$URL" ]; then
    warn "No artifact URL resolved; falling back to source build"
    FROM_SOURCE=1
  else
    info "Downloading $URL"
    # --retry 3 with exponential backoff (curl default) absorbs transient
    # CDN blips and connection resets. --retry-connrefused covers the
    # common "GitHub CDN just woke up" 5s window. The 300s max-time still
    # caps the whole transfer.
    if ! run_with_spinner "Downloading $TAR" \
        curl -fsSL --max-time 300 --retry 3 --retry-delay 2 --retry-connrefused \
        ${PROXY_ARGS[@]+"${PROXY_ARGS[@]}"} "$URL" -o "$TMP/$TAR"; then
      warn "Artifact download failed; falling back to build-from-source"
      FROM_SOURCE=1
    fi
  fi
fi

if [ "$FROM_SOURCE" -eq 1 ]; then
  build_from_source
else
  # Checksum verification
  if [ "$NO_CHECKSUM" -eq 1 ]; then
    warn "Verification skipped (--no-verify)"
  elif [ -n "$OFFLINE_TARBALL" ] && [ -z "$CHECKSUM" ]; then
    warn "Offline tarball + no --checksum supplied; skipping checksum verification"
  else
    if [ -z "$CHECKSUM" ]; then
      [ -z "$CHECKSUM_URL" ] && CHECKSUM_URL="${URL}.sha256"
      info "Fetching checksum from $CHECKSUM_URL"
      if ! curl -fsSL --max-time 30 ${PROXY_ARGS[@]+"${PROXY_ARGS[@]}"} "$CHECKSUM_URL" -o "$TMP/checksum.sha256"; then
        err "Checksum required and could not be fetched"
        err "Use --no-verify to skip checksum verification (not recommended)"
        exit 1
      fi
      CHECKSUM=$(awk '{print $1}' "$TMP/checksum.sha256")
      [ -n "$CHECKSUM" ] || { err "Empty checksum file"; exit 1; }
    fi
    verify_checksum "$TMP/$TAR" "$CHECKSUM" || { err "Installation aborted"; exit 1; }
    if [ -n "$URL" ]; then
      verify_sigstore_bundle "$TMP/$TAR" "$URL" || { err "Signature verification failed"; exit 1; }
    fi
  fi

  # Extract
  info "Extracting $TAR"
  if ! tar -xf "$TMP/$TAR" -C "$TMP"; then
    err "Failed to extract $TAR — archive may be corrupt or truncated"
    err "If the download was interrupted, retry; otherwise file an issue at:"
    err "  https://github.com/${OWNER}/${REPO}/issues"
    exit 1
  fi
  BIN="$TMP/ft"
  if [ ! -x "$BIN" ]; then
    BIN=$(find "$TMP" -maxdepth 3 -type f -name "ft" -perm -111 2>/dev/null | head -n 1)
  fi
  [ -x "$BIN" ] || { err "ft binary not found in tarball"; exit 1; }
  install -m 0755 "$BIN" "$DEST/ft"
  ok "Installed ft → $DEST/ft"
fi

# ───────────────────────────────────────────────────────────────────────────
# Post-install
# ───────────────────────────────────────────────────────────────────────────
maybe_add_path

if [ "$WITH_FONT" -eq 1 ]; then
  install_pragmasevka
fi

if [ "$VERIFY" -eq 1 ]; then
  info "Running \`ft doctor --json\` (informational; non-zero exit is OK on first install)"
  set +e
  if [ "$QUIET" -eq 1 ]; then
    # In quiet mode we just want a yes/no on "did the binary launch and emit
    # parseable JSON?" — don't dump the doctor body to stdout.
    "$DEST/ft" doctor --json >/dev/null 2>&1
  else
    "$DEST/ft" doctor --json 2>/dev/null | head -40
  fi
  set -e
  ok "Self-test invoked"
fi

# Resolved version for the summary box
if [ -x "$DEST/ft" ]; then
  RESOLVED_VERSION=$("$DEST/ft" --version 2>/dev/null | head -1 || echo "ft (unknown version)")
else
  RESOLVED_VERSION="ft $VERSION"
fi

# ───────────────────────────────────────────────────────────────────────────
# Final summary
# ───────────────────────────────────────────────────────────────────────────
if [ "$QUIET" -eq 0 ]; then
  summary_lines=()
  summary_lines+=("\033[1;32mFrankenTerm installed\033[0m")
  summary_lines+=("")
  summary_lines+=("Binary:   $DEST/ft")
  summary_lines+=("Version:  $RESOLVED_VERSION")
  if [ -n "${TARGET:-}" ]; then
    summary_lines+=("Platform: ${OS}/${ARCH} ($TARGET)")
  else
    summary_lines+=("Platform: ${OS}/${ARCH}")
  fi
  if [ "$WITH_FONT" -eq 1 ]; then
    summary_lines+=("Font:     Pragmasevka NF installed")
  fi
  summary_lines+=("")
  summary_lines+=("Quick start:")
  summary_lines+=("  ft --help               Show all subcommands")
  summary_lines+=("  ft version --full       Build metadata (commit / rustc / features)")
  summary_lines+=("  ft doctor --json        Diagnostic snapshot")
  summary_lines+=("  ft session list         Inspect running sessions")
  summary_lines+=("")
  summary_lines+=("Uninstall:")
  summary_lines+=("  rm $DEST/ft")
  if [ "$WITH_FONT" -eq 1 ]; then
    # Select the right font path based on the platform we installed for —
    # don't concatenate Linux + macOS paths together.
    case "$OS" in
      linux)  summary_lines+=("  rm -rf ${XDG_DATA_HOME:-$HOME/.local/share}/fonts/pragmasevka") ;;
      darwin) summary_lines+=("  rm -rf $HOME/Library/Fonts/pragmasevka") ;;
    esac
  fi
  summary_lines+=("")
  summary_lines+=("Docs:     https://github.com/${OWNER}/${REPO}")
  echo
  draw_box "0;32" ${summary_lines[@]+"${summary_lines[@]}"}
  echo
fi
