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
#   --no-app           macOS: skip the FrankenTerm.app GUI bundle install
#   --with-app         macOS: force the FrankenTerm.app GUI bundle install
#   --app-dest DIR     macOS: install FrankenTerm.app to DIR (default /Applications)
#   --from-source      Build from source instead of downloading binary
#   --quiet            Suppress non-error output
#   --no-gum           Disable gum formatting even if available
#   --no-verify        Skip checksum + signature verification (for testing only)
#   --offline TARBALL  Skip network entirely; install from local tarball
#   --force            Force reinstall even if same version is installed
#   --help             Show this message
#
# Environment overrides:
#   VERSION, OWNER, REPO, DEST, APP_DEST, ARTIFACT_URL, CHECKSUM, CHECKSUM_URL,
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
# macOS GUI app (.app) install. -1 = auto (on for darwin-arm64 prebuilt
# installs), 0 = disabled (--no-app), 1 = forced (--with-app). APP_DEST
# overrides the install directory (default /Applications, fallback
# ~/Applications when /Applications isn't writable). APP_ASSET is the
# published bundle archive; APP_INSTALLED_PATH is set on success for the
# final summary box.
INSTALL_APP=-1
APP_DEST="${APP_DEST:-}"
APP_ASSET="FrankenTerm-darwin-arm64.app.tar.xz"
APP_INSTALLED_PATH=""
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
  # 100MB headroom. The atomic Unix archive contains both ft and the matching
  # frankenterm-mux-server; download, extraction, staged install, and preserved
  # previous binaries can coexist briefly under $TMP and $DEST.
  local min_kb=102400
  local path="$DEST"
  [ ! -d "$path" ] && path=$(dirname "$path")
  if command -v df >/dev/null 2>&1; then
    local avail_kb
    avail_kb=$(df -Pk "$path" 2>/dev/null | awk 'NR==2 {print $4}')
    if [ -n "$avail_kb" ] && [ "$avail_kb" -lt "$min_kb" ]; then
      err "Insufficient disk space in $path (need at least 100MB)"
      exit 1
    fi
  else
    warn "df not found; skipping disk space check"
  fi
}

move_no_clobber() {
  local source="$1"
  local target="$2"

  [ ! -e "$target" ] && [ ! -L "$target" ] || return 1
  mv -n "$source" "$target" || return 1
  [ ! -e "$source" ] && [ ! -L "$source" ] || return 1
  [ -e "$target" ] || [ -L "$target" ]
}

restore_preserved_component() {
  local backup="$1"
  local target="$2"
  local label="$3"

  [ -z "$backup" ] && return 0
  if [ -e "$target" ] || [ -L "$target" ]; then
    err "Cannot restore preserved $label because its target path is occupied: $target"
    return 1
  fi
  if ! move_no_clobber "$backup" "$target"; then
    err "Failed to restore preserved $label from $backup to $target"
    return 1
  fi
}

install_process_family() {
  local ft_source="$1"
  local mux_source="$2"
  local ft_target="$DEST/ft"
  local mux_target="$DEST/frankenterm-mux-server"
  local ft_stage="${ft_target}.installing-$$"
  local mux_stage="${mux_target}.installing-$$"
  local ft_failed=""
  local stamp
  local ft_backup=""
  local mux_backup=""
  local ft_backup_candidate=""
  local mux_backup_candidate=""
  local recovery_failed=0
  stamp=$(date +%Y%m%d%H%M%S)
  ft_failed="${ft_target}.failed-publish-${stamp}-$$"
  ft_backup_candidate="${ft_target}.previous-${stamp}-$$"
  mux_backup_candidate="${mux_target}.previous-${stamp}-$$"

  if [ ! -f "$ft_source" ] || [ -L "$ft_source" ] || \
     [ ! -f "$mux_source" ] || [ -L "$mux_source" ]; then
    err "Refusing process-family source paths that are symlinks or non-regular files"
    return 1
  fi

  if [ -e "$ft_stage" ] || [ -L "$ft_stage" ] || \
     [ -e "$mux_stage" ] || [ -L "$mux_stage" ] || \
     [ -e "$ft_failed" ] || [ -L "$ft_failed" ] || \
     [ -e "$ft_backup_candidate" ] || [ -L "$ft_backup_candidate" ] || \
     [ -e "$mux_backup_candidate" ] || [ -L "$mux_backup_candidate" ]; then
    err "Refusing to reuse an existing process-family transaction path"
    return 1
  fi
  if { [ -e "$ft_target" ] || [ -L "$ft_target" ]; } && \
     { [ ! -f "$ft_target" ] || [ -L "$ft_target" ]; }; then
    err "Refusing to replace non-regular or symlink ft target at $ft_target"
    return 1
  fi
  if { [ -e "$mux_target" ] || [ -L "$mux_target" ]; } && \
     { [ ! -f "$mux_target" ] || [ -L "$mux_target" ]; }; then
    err "Refusing to replace non-regular or symlink frankenterm-mux-server target at $mux_target"
    return 1
  fi

  # Stage both binaries before changing either live name. A running mux keeps
  # its old executable inode; the new server bytes are picked up only after an
  # explicit, operator-controlled restart. Preserve previous bytes so a failed
  # publish or a breaking-codec rollback remains recoverable.
  if ! install -m 0755 "$ft_source" "$ft_stage"; then
    err "Failed to stage ft at $ft_stage"
    return 1
  fi
  if ! install -m 0755 "$mux_source" "$mux_stage"; then
    err "Failed to stage frankenterm-mux-server at $mux_stage"
    return 1
  fi
  if ! "$ft_stage" --version >/dev/null 2>&1; then
    err "Staged ft failed its launch/version probe; installed bytes are unchanged"
    return 1
  fi
  if ! "$mux_stage" --version >/dev/null 2>&1; then
    err "Staged frankenterm-mux-server failed its launch/version probe; installed bytes are unchanged"
    return 1
  fi

  if [ -e "$ft_target" ] || [ -L "$ft_target" ]; then
    if [ ! -f "$ft_target" ] || [ -L "$ft_target" ]; then
      err "Refusing to replace non-regular or symlink ft target at $ft_target"
      return 1
    fi
    ft_backup="$ft_backup_candidate"
    if [ -e "$ft_backup" ] || [ -L "$ft_backup" ]; then
      err "Refusing to overwrite existing ft backup at $ft_backup"
      return 1
    fi
    if ! move_no_clobber "$ft_target" "$ft_backup"; then
      err "Failed to preserve existing ft at $ft_backup"
      return 1
    fi
  fi
  if [ -e "$mux_target" ] || [ -L "$mux_target" ]; then
    if [ ! -f "$mux_target" ] || [ -L "$mux_target" ]; then
      err "Refusing to replace non-regular or symlink frankenterm-mux-server target at $mux_target"
      if ! restore_preserved_component "$ft_backup" "$ft_target" "ft"; then
        recovery_failed=1
      fi
      [ "$recovery_failed" -eq 0 ] || err "Process-family rollback was incomplete; preserved bytes require operator recovery"
      return 1
    fi
    mux_backup="$mux_backup_candidate"
    if [ -e "$mux_backup" ] || [ -L "$mux_backup" ]; then
      err "Refusing to overwrite existing frankenterm-mux-server backup at $mux_backup"
      if ! restore_preserved_component "$ft_backup" "$ft_target" "ft"; then
        recovery_failed=1
      fi
      [ "$recovery_failed" -eq 0 ] || err "Process-family rollback was incomplete; preserved bytes require operator recovery"
      return 1
    fi
    if ! move_no_clobber "$mux_target" "$mux_backup"; then
      err "Failed to preserve existing frankenterm-mux-server at $mux_backup"
      if ! restore_preserved_component "$ft_backup" "$ft_target" "ft"; then
        recovery_failed=1
      fi
      [ "$recovery_failed" -eq 0 ] || err "Process-family rollback was incomplete; preserved bytes require operator recovery"
      return 1
    fi
  fi

  if ! move_no_clobber "$ft_stage" "$ft_target"; then
    err "Failed to publish staged ft at $ft_target"
    if ! restore_preserved_component "$ft_backup" "$ft_target" "ft"; then
      recovery_failed=1
    fi
    if ! restore_preserved_component "$mux_backup" "$mux_target" "frankenterm-mux-server"; then
      recovery_failed=1
    fi
    [ "$recovery_failed" -eq 0 ] || err "Process-family rollback was incomplete; preserved bytes require operator recovery"
    return 1
  fi
  if ! move_no_clobber "$mux_stage" "$mux_target"; then
    err "Failed to publish staged frankenterm-mux-server at $mux_target"
    if ! move_no_clobber "$ft_target" "$ft_failed"; then
      err "Failed to quarantine newly published ft at $ft_failed"
      recovery_failed=1
    fi
    if ! restore_preserved_component "$ft_backup" "$ft_target" "ft"; then
      recovery_failed=1
    fi
    if ! restore_preserved_component "$mux_backup" "$mux_target" "frankenterm-mux-server"; then
      recovery_failed=1
    fi
    [ "$recovery_failed" -eq 0 ] || err "Process-family rollback was incomplete; preserved bytes require operator recovery"
    return 1
  fi

  ok "Installed ft → $ft_target"
  ok "Installed frankenterm-mux-server → $mux_target"
  if [ -n "$ft_backup" ]; then info "Previous ft preserved at $ft_backup"; fi
  if [ -n "$mux_backup" ]; then
    info "Previous frankenterm-mux-server preserved at $mux_backup"
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
  # Fail closed before invoking either installed component or entering a
  # pipeline.  A missing pipeline utility must not be discovered only after one
  # side of the process family has already been executed.
  command -v head >/dev/null 2>&1 || return 1
  command -v grep >/dev/null 2>&1 || return 1
  command -v sed >/dev/null 2>&1 || return 1
  command -v sort >/dev/null 2>&1 || return 1
  command -v awk >/dev/null 2>&1 || return 1
  [ -f "$DEST/ft" ] && [ ! -L "$DEST/ft" ] && [ -x "$DEST/ft" ] || return 1
  [ -f "$DEST/frankenterm-mux-server" ] && \
    [ ! -L "$DEST/frankenterm-mux-server" ] && \
    [ -x "$DEST/frankenterm-mux-server" ] || return 1
  local ft_cur mux_cur ft_identity mux_identity expected_identity_suffix
  ft_cur=$("$DEST/ft" --version 2>/dev/null | head -1 | awk '{print $2}' || echo "")
  mux_cur=$("$DEST/frankenterm-mux-server" --version 2>/dev/null | head -1 | awk '{print $2}' || echo "")
  [ -z "$ft_cur" ] && return 1
  [ -z "$mux_cur" ] && return 1
  # Strip leading 'v' from target_ver for comparison ("v0.2.0" vs "0.2.0")
  local stripped="${target_ver#v}"
  { [ "$ft_cur" = "$stripped" ] || [ "$ft_cur" = "$target_ver" ]; } || return 1
  { [ "$mux_cur" = "$stripped" ] || [ "$mux_cur" = "$target_ver" ]; } || return 1

  # Semver is not a process-family identity: two rebuilds of the same tag can
  # carry different codec or source bytes. Admit the fast path only when each
  # binary exposes one sealed marker and those markers differ solely by role.
  ft_identity=$(LC_ALL=C grep -aoE \
    'FT_ATOMIC_COMPONENT_IDENTITY_V1:[0-9a-f]{64}:ft:[A-Za-z0-9][A-Za-z0-9._+-]*:[A-Za-z0-9][A-Za-z0-9._+-]*:[A-Za-z0-9][A-Za-z0-9._+-]*;' \
    "$DEST/ft" 2>/dev/null | sed 's/:ft:/:ROLE:/' | sort -u || true)
  mux_identity=$(LC_ALL=C grep -aoE \
    'FT_ATOMIC_COMPONENT_IDENTITY_V1:[0-9a-f]{64}:frankenterm-mux-server:[A-Za-z0-9][A-Za-z0-9._+-]*:[A-Za-z0-9][A-Za-z0-9._+-]*:[A-Za-z0-9][A-Za-z0-9._+-]*;' \
    "$DEST/frankenterm-mux-server" 2>/dev/null | \
    sed 's/:frankenterm-mux-server:/:ROLE:/' | sort -u || true)
  [ -n "$ft_identity" ] || return 1
  [ -n "$mux_identity" ] || return 1
  [ "$(printf '%s\n' "$ft_identity" | awk 'END { print NR }')" -eq 1 ] || return 1
  [ "$(printf '%s\n' "$mux_identity" | awk 'END { print NR }')" -eq 1 ] || return 1
  [ "$ft_identity" = "$mux_identity" ] || return 1
  # Matching stale or non-shipping markers are still not the requested release.
  # TARGET is resolved by detect_platform before this function can run.
  [ -n "${TARGET:-}" ] || return 1
  expected_identity_suffix="${TARGET}:release-interactive:${stripped};"
  [ "${ft_identity##*:ROLE:}" = "$expected_identity_suffix" ]
}

# ───────────────────────────────────────────────────────────────────────────
# PATH integration
# ───────────────────────────────────────────────────────────────────────────
maybe_add_path() {
  case ":$PATH:" in
    *:"$DEST":*) return 0 ;;
    *)
      if [ "$EASY" -eq 1 ]; then
        # The exact line we'd write — used both for the duplicate-check
        # grep AND the printf below, so the two can't drift out of sync.
        # shellcheck disable=SC2016
        # ^ The literal `$PATH` is intentional — it must stay as a
        #   shell-variable reference to be expanded at the user's
        #   shell startup, not interpolated here at install time.
        local export_line
        export_line="export PATH=\"$DEST:\$PATH\""
        local appended_to=0
        local probed_count=0
        for rc in "$HOME/.zshrc" "$HOME/.bashrc"; do
          if [ -e "$rc" ] && [ -w "$rc" ]; then
            probed_count=$((probed_count + 1))
            # `grep -Fx` matches the whole line as a fixed string —
            # so a bare mention of $DEST in a comment doesn't fool us
            # into skipping a real export, and we don't get tripped by
            # PATHs that have $DEST as a substring of a longer path.
            if ! grep -Fxq "$export_line" "$rc" 2>/dev/null; then
              # Leading newline guards against rc files without a
              # trailing newline (rare but valid POSIX text files).
              printf '\n# Added by FrankenTerm installer\n%s\n' \
                "$export_line" >> "$rc"
              appended_to=$((appended_to + 1))
            fi
          fi
        done
        if [ "$appended_to" -gt 0 ]; then
          warn "PATH export appended to $appended_to shell rc file(s); restart shell to use ft"
        elif [ "$probed_count" -gt 0 ]; then
          # Files exist and are writable, but the export was already
          # present — nothing to do.
          info "PATH export already present in shell rc; no changes made"
        else
          warn "No writable ~/.zshrc or ~/.bashrc found; add $DEST to PATH manually"
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
# macOS GUI app (FrankenTerm.app) install
#
# Default-on for darwin/arm64 prebuilt installs (the published .app asset only
# exists for that target). Downloads the signed bundle, places it in
# /Applications (or ~/Applications without admin rights), registers it with
# LaunchServices, and refreshes the Dock so an existing Dock pin / Spotlight /
# Launchpad resolve to the new version. It does NOT add a new Dock tile — app
# pinning is a user gesture, not an installer's job.
# ───────────────────────────────────────────────────────────────────────────
should_install_app() {
  # Explicit opt-out always wins.
  [ "$INSTALL_APP" -eq 0 ] && return 1
  # GUI app is macOS-only.
  if [ "${OS:-}" != "darwin" ]; then
    [ "$INSTALL_APP" -eq 1 ] && warn "--with-app ignored: the FrankenTerm GUI app is macOS-only"
    return 1
  fi
  # Only the arm64 prebuilt bundle is published.
  if [ "${ARCH:-}" != "aarch64" ]; then
    [ "$INSTALL_APP" -eq 1 ] && warn "--with-app ignored: no prebuilt FrankenTerm.app for ${OS}/${ARCH}; build it with scripts/create-macos-bundle.sh"
    return 1
  fi
  # Source builds and offline mode have no published .app to fetch.
  if [ "$FROM_SOURCE" -eq 1 ]; then
    [ "$INSTALL_APP" -eq 1 ] && warn "--with-app ignored for source builds; run scripts/create-macos-bundle.sh after building"
    return 1
  fi
  if [ -n "$OFFLINE_TARBALL" ]; then
    [ "$INSTALL_APP" -eq 1 ] && warn "--with-app ignored in --offline mode (no network for the .app asset)"
    return 1
  fi
  return 0
}

install_macos_app() {
  local app_url dest tmp_app_tar extracted_app target_app staged_app backup_app
  app_url="https://github.com/${OWNER}/${REPO}/releases/download/${VERSION}/${APP_ASSET}"

  # Destination: explicit --app-dest, else /Applications when writable, else
  # ~/Applications (no sudo). ~/Applications is a first-class macOS app dir.
  dest="$APP_DEST"
  if [ -z "$dest" ]; then
    if [ -w "/Applications" ]; then
      dest="/Applications"
    else
      dest="$HOME/Applications"
      info "/Applications not writable; installing app to $dest"
    fi
  fi
  if ! mkdir -p "$dest" 2>/dev/null; then
    warn "Could not create app destination $dest; skipping GUI app install"
    return 0
  fi

  info "Downloading FrankenTerm.app from $app_url"
  tmp_app_tar="$TMP/$APP_ASSET"
  if ! run_with_spinner "Downloading $APP_ASSET" \
      curl -fsSL --max-time 300 --retry 3 --retry-delay 2 --retry-connrefused \
      ${PROXY_ARGS[@]+"${PROXY_ARGS[@]}"} "$app_url" -o "$tmp_app_tar"; then
    warn "FrankenTerm.app asset not found at $app_url; skipping GUI app install"
    warn "(The ft CLI install above is unaffected.)"
    return 0
  fi

  # Checksum: fail CLOSED, same posture as the ft CLI. The .app is executable
  # code going into /Applications, so we never install it unverified. A missing
  # sidecar or a mismatch skips the GUI app (the ft CLI install is unaffected)
  # rather than aborting the whole installer; --no-verify is the explicit
  # escape hatch. The release workflow always publishes the sidecar.
  if [ "$NO_CHECKSUM" -eq 1 ]; then
    warn "App checksum verification skipped (--no-verify)"
  else
    local app_sum=""
    if curl -fsSL --max-time 30 ${PROXY_ARGS[@]+"${PROXY_ARGS[@]}"} "${app_url}.sha256" -o "$TMP/app.sha256" 2>/dev/null; then
      app_sum=$(awk '{print $1}' "$TMP/app.sha256")
    fi
    if [ -n "$app_sum" ]; then
      verify_checksum "$tmp_app_tar" "$app_sum" || { err "FrankenTerm.app checksum failed; skipping GUI app install"; return 0; }
    else
      warn "Could not fetch the FrankenTerm.app checksum; skipping GUI app install"
      warn "(Pass --no-verify to install the app without checksum verification.)"
      return 0
    fi
  fi

  info "Extracting FrankenTerm.app"
  if ! tar -xf "$tmp_app_tar" -C "$TMP"; then
    warn "Failed to extract $APP_ASSET; skipping GUI app install"
    return 0
  fi
  extracted_app="$TMP/FrankenTerm.app"
  if [ ! -d "$extracted_app" ]; then
    extracted_app=$(find "$TMP" -maxdepth 3 -type d -name "FrankenTerm.app" 2>/dev/null | head -n 1)
  fi
  if [ -z "$extracted_app" ] || [ ! -d "$extracted_app" ]; then
    warn "FrankenTerm.app not found in $APP_ASSET; skipping GUI app install"
    return 0
  fi

  target_app="$dest/FrankenTerm.app"
  staged_app="${target_app}.installing-$$"
  backup_app=""
  if [ -e "$staged_app" ] || [ -L "$staged_app" ]; then
    warn "Refusing to reuse existing app staging path $staged_app"
    return 0
  fi
  if { [ -e "$target_app" ] || [ -L "$target_app" ]; } && \
     { [ ! -d "$target_app" ] || [ -L "$target_app" ]; }; then
    warn "Refusing to replace non-directory or symlink app target at $target_app"
    return 0
  fi
  # Copy to a sibling staging path before changing the live bundle. This keeps
  # updates recoverable and avoids deleting a working app before the new bundle
  # has been copied successfully.
  if command -v ditto >/dev/null 2>&1; then
    if ! ditto "$extracted_app" "$staged_app"; then
      warn "Failed to stage FrankenTerm.app at $staged_app; leaving the current app unchanged"
      return 0
    fi
  else
    if ! cp -R "$extracted_app" "$staged_app"; then
      warn "Failed to stage FrankenTerm.app at $staged_app; leaving the current app unchanged"
      return 0
    fi
  fi

  if [ -e "$target_app" ] || [ -L "$target_app" ]; then
    backup_app="${target_app}.previous-$(date +%Y%m%d%H%M%S)-$$"
    if [ -e "$backup_app" ] || [ -L "$backup_app" ]; then
      warn "Refusing to overwrite existing app backup at $backup_app"
      return 0
    fi
    if ! move_no_clobber "$target_app" "$backup_app"; then
      warn "Could not preserve existing $target_app; staged app remains at $staged_app"
      return 0
    fi
  fi
  if ! move_no_clobber "$staged_app" "$target_app"; then
    warn "Could not publish staged FrankenTerm.app at $target_app"
    if [ -n "$backup_app" ] && [ ! -e "$target_app" ] && [ ! -L "$target_app" ]; then
      if ! move_no_clobber "$backup_app" "$target_app"; then
        err "App rollback was incomplete; preserved bundle requires operator recovery at $backup_app"
      fi
    fi
    return 0
  fi

  # A terminal/curl-placed bundle isn't Gatekeeper-quarantined, but strip the
  # attribute defensively so a proxy/CDN that tagged the download can't force a
  # first-launch prompt.
  xattr -dr com.apple.quarantine "$target_app" 2>/dev/null || true

  # Register with LaunchServices — the step a Finder drag-to-Applications does
  # for free — so an existing Dock pin, Spotlight, and Launchpad resolve to the
  # new bundle.
  local lsreg="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
  if [ -x "$lsreg" ]; then
    "$lsreg" -f "$target_app" >/dev/null 2>&1 || true
  fi

  # Refresh (not re-pin) the Dock so a pinned tile rebinds to the new bundle.
  # Only when a Dock is actually running (a GUI login session); the Dock
  # relaunches itself. Never adds a tile.
  if pgrep -x Dock >/dev/null 2>&1; then
    killall Dock >/dev/null 2>&1 || true
  fi

  ok "Installed FrankenTerm.app → $target_app"
  if [ -n "$backup_app" ]; then
    info "Previous FrankenTerm.app preserved at $backup_app"
  fi
  APP_INSTALLED_PATH="$target_app"
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
  command -v python3 >/dev/null 2>&1 || {
    err "python3 is required to seal and verify a source-built process family"
    exit 1
  }
  # A source fallback must preserve the exact requested release identity. A
  # typo, missing tag, or transient clone failure must not silently build the
  # default branch and publish different client/server bytes under the requested
  # version label.
  if ! git clone --depth 1 --branch "$VERSION" \
       "https://github.com/${OWNER}/${REPO}.git" "$TMP/src" 2>/dev/null; then
    err "Failed to clone exact release tag $VERSION from ${OWNER}/${REPO}"
    err "Refusing to substitute the default branch for an immutable process-family identity."
    err "Check network connectivity and confirm that the release tag exists."
    exit 1
  fi
  # Build the CLI and mux server from the same source identity. Remote-domain
  # releases are an atomic process family; a client-only fallback would recreate
  # the exact codec-stranding failure that the prebuilt archives prevent.
  local panic_contract_tool="$TMP/src/scripts/check-release-panic-contract.sh"
  if [ ! -f "$panic_contract_tool" ] || \
     ! bash "$panic_contract_tool" --profiles-only; then
    err "Source tree does not satisfy the shipped panic-profile contract."
    exit 1
  fi
  local atomic_tool="$TMP/src/scripts/atomic-component-manifest.sh"
  if [ ! -f "$atomic_tool" ] || [ -L "$atomic_tool" ]; then
    err "Atomic component manifest tool is missing or unsafe: $atomic_tool"
    err "Refusing to build an unverifiable CLI/mux-server process family."
    exit 1
  fi

  # Derive the same commit/build identity used by release CI.  The explicit
  # target is important: Cargo configuration can otherwise select a different
  # target after we have already frozen the identity that the binaries claim.
  local source_revision=""
  local workspace_version=""
  local build_target=""
  local build_profile="release-interactive"
  local feature_contract="process-family-ft-mux-server-default-features-v1"
  local build_id=""
  source_revision=$(git -C "$TMP/src" rev-parse HEAD 2>/dev/null) || {
    err "Cannot resolve the exact source revision for the cloned release tag."
    exit 1
  }
  workspace_version=$(awk '
    /^\[workspace.package\]$/ { in_workspace_package = 1; next }
    in_workspace_package && /^version = / {
      gsub(/^version = "/, "")
      gsub(/".*/, "")
      print
      exit
    }
  ' "$TMP/src/Cargo.toml")
  if [ -z "$workspace_version" ] || [ "${VERSION#v}" != "$workspace_version" ]; then
    err "Release tag $VERSION does not match workspace version ${workspace_version:-<missing>}."
    err "Refusing to mint a misleading source-build identity."
    exit 1
  fi
  build_target=$(rustc -vV 2>/dev/null | sed -n 's/^host: //p' | head -n 1)
  if [[ ! "$build_target" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]]; then
    err "Cannot derive a canonical native Rust target for the source build."
    exit 1
  fi
  if ! build_id=$(bash "$atomic_tool" derive-build-id \
      --source-revision "$source_revision" \
      --version "$workspace_version" \
      --target "$build_target" \
      --profile "$build_profile" \
      --feature-contract "$feature_contract"); then
    err "Failed to derive the canonical atomic source-build identity."
    exit 1
  fi

  # Friendly error wrapping: a bare `set -e` exit on cargo failure would
  # not give the user any actionable diagnosis.
  if ! ( cd "$TMP/src" && env \
      CARGO_TARGET_DIR="$TMP/src/target" \
      FT_ATOMIC_BUILD_IDENTITY="$build_id" \
      FT_ATOMIC_BUILD_PROFILE="$build_profile" \
      cargo build --locked --profile "$build_profile" --target "$build_target" \
      -p frankenterm --bin ft \
      -p frankenterm-mux-server --bin frankenterm-mux-server ); then
    err "Source build failed."
    err "Common causes:"
    err "  - Missing system deps on Linux: pkg-config, libcairo2-dev,"
    err "    libx11-dev, libx11-xcb-dev, libxcb-util-dev, libxcb-image0-dev,"
    err "    libxkbcommon-dev, libxkbcommon-x11-dev."
    err "  - Out-of-disk during compile (cargo's target/ uses 10+ GB)."
    err "  - Missing or stale nightly toolchain (see rust-toolchain.toml)."
    exit 1
  fi
  local bin="$TMP/src/target/$build_target/$build_profile/ft"
  local mux_bin="$TMP/src/target/$build_target/$build_profile/frankenterm-mux-server"
  [ -x "$bin" ] || { err "Build did not produce $bin"; exit 1; }
  [ -x "$mux_bin" ] || { err "Build did not produce $mux_bin"; exit 1; }

  # Verify the embedded role/build/target/profile/version markers before any
  # live destination is mutated.  A version probe alone cannot distinguish a
  # sealed atomic family from the ordinary `unsealed` development default.
  local proof_root="$TMP/source-family-proof"
  local proof_manifest="$proof_root/source-family.component-manifest.json"
  if ! mkdir "$proof_root" \
      || ! install -m 0755 "$bin" "$proof_root/ft" \
      || ! install -m 0755 "$mux_bin" "$proof_root/frankenterm-mux-server"; then
    err "Failed to stage the source-built process family for identity proof."
    exit 1
  fi
  if ! bash "$atomic_tool" generate \
      --root "$proof_root" \
      --source-root "$TMP/src" \
      --output "$proof_manifest" \
      --build-id "$build_id" \
      --source-revision "$source_revision" \
      --version "$workspace_version" \
      --target "$build_target" \
      --profile "$build_profile" \
      --feature-contract "$feature_contract" \
      --entry executable:cli:ft:ft \
      --entry executable:mux-server:frankenterm-mux-server:frankenterm-mux-server \
      || ! bash "$atomic_tool" verify \
      --root "$proof_root" \
      --manifest "$proof_manifest"; then
    err "Source-built ft and mux-server do not form one sealed atomic family."
    err "Installed bytes are unchanged."
    exit 1
  fi
  install_process_family "$proof_root/ft" "$proof_root/frankenterm-mux-server"
}

# ───────────────────────────────────────────────────────────────────────────
# Usage + arg parsing
# ───────────────────────────────────────────────────────────────────────────
usage() {
  cat <<EOFU
Usage: install.sh [--version vX.Y.Z] [--dest DIR] [--system] [--easy-mode]
                  [--verify] [--with-font] [--no-app] [--with-app]
                  [--app-dest DIR] [--from-source] [--quiet]
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
  --no-app           macOS: skip the FrankenTerm.app GUI bundle install
  --with-app         macOS: force the FrankenTerm.app GUI bundle install
  --app-dest DIR     macOS: install FrankenTerm.app to DIR (default /Applications)
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
  VERSION, OWNER, REPO, DEST, APP_DEST, ARTIFACT_URL, CHECKSUM, CHECKSUM_URL,
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
    --no-app) INSTALL_APP=0; shift ;;
    --with-app) INSTALL_APP=1; shift ;;
    --app-dest) require_option_value "$1" "${2:-}"; APP_DEST="$2"; shift 2 ;;
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

# Already at target version short-circuit (unless --force or offline). This
# check belongs inside the installer lock: otherwise a concurrent same-semver
# process-family publication can be observed between its two canonical moves.
# check_installed_version also requires one exact sealed build identity across
# both roles; matching --version output alone is not sufficient.
if [ "$FORCE_INSTALL" -eq 0 ] && [ -z "$OFFLINE_TARBALL" ] && [ -n "$VERSION" ] \
    && check_installed_version "$VERSION"; then
  ok "ft + frankenterm-mux-server $VERSION are already installed at $DEST"
  info "Use --force to reinstall"
  # Still honour the font / GUI-app side installs even when the CLI is current,
  # so a re-run can add the .app to an existing CLI-only install. Decide once
  # (should_install_app may warn under --with-app) to avoid a double call.
  app_wanted=0
  if should_install_app; then app_wanted=1; fi
  if [ "$WITH_FONT" -eq 1 ] || [ "$app_wanted" -eq 1 ]; then
    TMP=$(mktemp -d)
    if [ "$WITH_FONT" -eq 1 ]; then install_pragmasevka; fi
    if [ "$app_wanted" -eq 1 ]; then install_macos_app; fi
  fi
  exit 0
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

  # Extract into an otherwise-empty package root.  The atomic manifest verifies
  # the complete inventory, so download/checksum sidecars in $TMP must not be
  # allowed to masquerade as package members (or make every valid archive fail).
  PACKAGE_ROOT="$TMP/package"
  if ! mkdir -p "$PACKAGE_ROOT"; then
    err "Failed to create package verification directory"
    exit 1
  fi
  info "Extracting $TAR"
  if ! tar -xf "$TMP/$TAR" -C "$PACKAGE_ROOT"; then
    err "Failed to extract $TAR — archive may be corrupt or truncated"
    err "If the download was interrupted, retry; otherwise file an issue at:"
    err "  https://github.com/${OWNER}/${REPO}/issues"
    exit 1
  fi

  # A checksum proves archive bytes, but it cannot prove that the CLI and mux
  # server came from one source/build identity.  Keep this atomic process-family
  # verification mandatory even when --no-verify skips transport authenticity.
  COMPONENT_VERIFIER="$PACKAGE_ROOT/verify-components.sh"
  COMPONENT_MANIFEST="$PACKAGE_ROOT/${ASSET%.tar.xz}.component-manifest.json"
  [ -f "$COMPONENT_VERIFIER" ] || {
    err "Atomic component verifier not found in tarball"
    err "Refusing an unverifiable client/server process-family install"
    exit 1
  }
  [ -f "$COMPONENT_MANIFEST" ] || {
    err "Atomic component manifest not found in tarball: $(basename "$COMPONENT_MANIFEST")"
    err "Refusing an unverifiable client/server process-family install"
    exit 1
  }
  info "Verifying atomic CLI/mux-server build identity"
  if ! bash "$COMPONENT_VERIFIER" verify \
      --root "$PACKAGE_ROOT" \
      --manifest "$COMPONENT_MANIFEST"; then
    err "Atomic component verification failed"
    err "Refusing to install mixed, incomplete, or corrupt process-family bytes"
    exit 1
  fi

  BIN="$PACKAGE_ROOT/ft"
  if [ ! -x "$BIN" ]; then
    BIN=$(find "$PACKAGE_ROOT" -maxdepth 3 -type f -name "ft" -perm -111 2>/dev/null | head -n 1)
  fi
  [ -x "$BIN" ] || { err "ft binary not found in tarball"; exit 1; }
  MUX_BIN="$PACKAGE_ROOT/frankenterm-mux-server"
  if [ ! -x "$MUX_BIN" ]; then
    MUX_BIN=$(find "$PACKAGE_ROOT" -maxdepth 3 -type f -name "frankenterm-mux-server" -perm -111 2>/dev/null | head -n 1)
  fi
  [ -x "$MUX_BIN" ] || {
    err "frankenterm-mux-server binary not found in tarball"
    err "Refusing a client-only install that could strand persistent remote domains"
    exit 1
  }
  install_process_family "$BIN" "$MUX_BIN"
fi

# ───────────────────────────────────────────────────────────────────────────
# Post-install
# ───────────────────────────────────────────────────────────────────────────
maybe_add_path

if [ "$WITH_FONT" -eq 1 ]; then
  install_pragmasevka
fi

if should_install_app; then
  install_macos_app
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
  summary_lines+=("Mux:      $DEST/frankenterm-mux-server")
  summary_lines+=("Version:  $RESOLVED_VERSION")
  if [ -n "${TARGET:-}" ]; then
    summary_lines+=("Platform: ${OS}/${ARCH} ($TARGET)")
  else
    summary_lines+=("Platform: ${OS}/${ARCH}")
  fi
  if [ "$WITH_FONT" -eq 1 ]; then
    summary_lines+=("Font:     Pragmasevka NF installed")
  fi
  if [ -n "$APP_INSTALLED_PATH" ]; then
    summary_lines+=("GUI app:  $APP_INSTALLED_PATH")
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
  summary_lines+=("  rm $DEST/frankenterm-mux-server")
  if [ "$WITH_FONT" -eq 1 ]; then
    # Select the right font path based on the platform we installed for —
    # don't concatenate Linux + macOS paths together.
    case "$OS" in
      linux)  summary_lines+=("  rm -rf ${XDG_DATA_HOME:-$HOME/.local/share}/fonts/pragmasevka") ;;
      darwin) summary_lines+=("  rm -rf $HOME/Library/Fonts/pragmasevka") ;;
    esac
  fi
  if [ -n "$APP_INSTALLED_PATH" ]; then
    summary_lines+=("  rm -rf $APP_INSTALLED_PATH")
  fi
  summary_lines+=("")
  summary_lines+=("Docs:     https://github.com/${OWNER}/${REPO}")
  echo
  draw_box "0;32" ${summary_lines[@]+"${summary_lines[@]}"}
  echo
fi
