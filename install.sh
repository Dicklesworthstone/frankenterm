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
#   --no-verify        Skip DSR minisign verification (SHA-256 remains required)
#   --offline TARBALL  Skip network entirely; install from local tarball
#   --force            Force reinstall even if same version is installed
#   --help             Show this message
#
# Environment overrides:
#   VERSION, OWNER, REPO, DEST, APP_DEST, ARTIFACT_URL, CHECKSUM, CHECKSUM_URL,
#   MINISIGN_SIGNATURE_URL,
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
NO_MINISIGN=0
FORCE_INSTALL=0
# --activate <generation-id>: promote a published candidate to the current
# process-family authority; requires --idle-host-confirmed (ft-xxfwy.3).
ACTIVATE_GENERATION=""
IDLE_HOST_CONFIRMED=0
# macOS GUI app (.app) install. -1 = auto (on for darwin-arm64 prebuilt
# installs), 0 = disabled (--no-app), 1 = forced (--with-app). APP_DEST
# overrides the install directory (default /Applications, fallback
# ~/Applications when /Applications isn't writable). APP_ASSET is the
# published bundle archive; APP_INSTALLED_PATH and APP_ACTIVATION_STATE report
# the exact current-or-pending app authority in the final summary box.
INSTALL_APP=-1
APP_DEST="${APP_DEST:-}"
APP_ASSET="FrankenTerm-darwin-arm64.app.tar.xz"
APP_INSTALLED_PATH=""
APP_ACTIVATION_STATE=""
APP_RECEIPT_REQUESTED="false"
APP_RECEIPT_RESULT="not_requested"
APP_RECEIPT_REASON="not_selected"
APP_RECEIPT_MANIFEST_ID=""
APP_RECEIPT_CANDIDATE_PATH=""
APP_RECEIPT_READINESS="not_run"
PENDING_PROCESS_FAMILY_GENERATION=""
PUBLISHED_PROCESS_FAMILY_VERSION=""
PUBLISHED_PROCESS_FAMILY_ROOT=""
PUBLISHED_PROCESS_FAMILY_VERIFIER_AUTHORITY=""
PROCESS_FAMILY_ACTIVATION_STATE=""
PROCESS_FAMILY_ACTIVE_AUTHORITY=""
PROCESS_FAMILY_ACTIVE_ROOT=""
PROCESS_FAMILY_PENDING_REASON=""
INITIAL_SELECTOR_HOLD_REASON=""
VERIFIED_ARCHIVE_IDENTITY=""
FONT_INSTALLED_PATH=""
OFFLINE_TARBALL=""
CHECKSUM="${CHECKSUM:-}"
CHECKSUM_URL="${CHECKSUM_URL:-}"
MINISIGN_SIGNATURE_URL="${MINISIGN_SIGNATURE_URL:-}"
APP_MINISIGN_SIGNATURE_URL="${APP_MINISIGN_SIGNATURE_URL:-}"
MINISIGN_PUBLIC_KEY="RWSoYi6NXJWzaRs1mJmOwwXrZfPWcq6MXnQlNMLBYKzlIQTLwuVQG6uO"
ARTIFACT_URL="${ARTIFACT_URL:-}"
LOCK_FILE="/tmp/ft-install.lock"
HARDCODED_FALLBACK_VERSION="v0.2.0"

# Download and extraction resource contracts. These are deliberately finite
# and are enforced both before transfer and again through descriptor-pinned
# post-download/extraction checks. The app allowance includes its bundled
# browser runtime; the standalone triplet has a substantially smaller budget.
MAX_PROCESS_ARCHIVE_BYTES=1073741824
MAX_PROCESS_EXPANDED_BYTES=4294967296
MAX_APP_ARCHIVE_BYTES=4294967296
MAX_APP_EXPANDED_BYTES=17179869184
MAX_FONT_ARCHIVE_BYTES=268435456
MAX_FONT_EXPANDED_BYTES=1073741824
INSTALLER_FREE_SPACE_HEADROOM_BYTES=67108864

# Cleanup state. The permanent lock inode is never unlinked; a Python holder
# owns its kernel advisory lock until the shell closes the control FIFO.
TMP=""
LOCKED=0
LOCK_HOLDER_PID=""
LOCK_CONTROL_FIFO=""
LOCK_READY_FILE=""
cleanup() {
  if [ -n "$TMP" ]; then
    rm -rf "$TMP"
  fi
  if [ "$LOCKED" -eq 1 ]; then
    exec 9>&- 2>/dev/null || true
    [ -n "$LOCK_HOLDER_PID" ] && wait "$LOCK_HOLDER_PID" 2>/dev/null || true
  fi
  if [ -n "$LOCK_CONTROL_FIFO" ]; then
    rm -f "$LOCK_CONTROL_FIFO"
  fi
  if [ -n "$LOCK_READY_FILE" ]; then
    rm -f "$LOCK_READY_FILE"
  fi
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
    gum style --foreground 196 "✗ $*" >&2
  else
    echo -e "\033[0;31m✗\033[0m $*" >&2
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
    warn "the ft CLI + mux server + PTY guardian work fine under WSL."
  fi

  # FrankenTerm release assets are named ft-{os}-{arch}.tar.xz where:
  #   arch ∈ {amd64, arm64}   — NOT Rust triples
  #   os   ∈ {linux, darwin}
  # Keep these names identical to the DSR release contract.
  ASSET=""
  TARGET="" # informational only — matches the DSR build target
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
require_filesystem_capacity() {
  local path="$1" required_bytes="$2" label="$3"
  [[ "$required_bytes" =~ ^[0-9]+$ ]] || {
    err "Invalid byte budget for $label"
    return 1
  }
  if ! python3 - "$path" "$required_bytes" "$label" <<'PY'
import os, stat, sys

path, required_raw, label = sys.argv[1:]
required = int(required_raw)
flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0)
fd = os.open(path, flags)
try:
    observed = os.fstat(fd)
    named = os.stat(path, follow_symlinks=False)
    if (not stat.S_ISDIR(observed.st_mode) or
            (observed.st_dev, observed.st_ino) != (named.st_dev, named.st_ino)):
        raise SystemExit(f"{label} capacity root is not one stable nofollow directory")
    filesystem = os.fstatvfs(fd)
    available = filesystem.f_bavail * filesystem.f_frsize
    if available < required:
        raise SystemExit(
            f"{label} has {available} free bytes but requires {required} bounded bytes"
        )
finally:
    os.close(fd)
PY
  then
    err "Insufficient or unsafe filesystem capacity for $label at $path"
    return 1
  fi
}

verify_bounded_download_file() {
  local path="$1" max_bytes="$2"
  python3 - "$path" "$max_bytes" <<'PY'
import os, stat, sys

path, maximum_raw = sys.argv[1:]
maximum = int(maximum_raw)
flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0)
fd = os.open(path, flags)
try:
    observed = os.fstat(fd)
    named = os.stat(path, follow_symlinks=False)
    if (not stat.S_ISREG(observed.st_mode) or observed.st_nlink != 1 or
            observed.st_size > maximum or
            (observed.st_dev, observed.st_ino) != (named.st_dev, named.st_ino)):
        raise SystemExit("download did not produce one bounded single-link regular file")
finally:
    os.close(fd)
PY
}

download_https_bounded() {
  local url="$1" output="$2" max_bytes="$3" max_time="$4" retry="${5:-0}"
  local output_parent required_bytes
  case "$url" in
    https://*) ;;
    *)
      err "Remote download authority must use HTTPS: $url"
      return 1
      ;;
  esac
  [[ "$max_bytes" =~ ^[0-9]+$ ]] && [[ "$max_time" =~ ^[0-9]+$ ]] || return 1
  [ ! -e "$output" ] && [ ! -L "$output" ] || {
    err "Refusing to overwrite a retained download path: $output"
    return 1
  }
  if ! curl --help all 2>/dev/null | LC_ALL=C grep -Fq -- '--max-filesize'; then
    err "curl cannot enforce --max-filesize; refusing an unbounded download"
    return 1
  fi
  output_parent=$(dirname "$output") || return 1
  required_bytes=$((max_bytes + INSTALLER_FREE_SPACE_HEADROOM_BYTES))
  require_filesystem_capacity "$output_parent" "$required_bytes" "temporary download" || return 1

  local curl_args=(-fsSL --proto '=https' --proto-redir '=https'
    --max-filesize "$max_bytes" --max-time "$max_time")
  if [ "$retry" -eq 1 ]; then
    curl_args+=(--retry 3 --retry-delay 2 --retry-connrefused)
  fi
  # curl's pathname-based -o open would leave a same-UID replacement window
  # between the absence check above and the transfer. Open the output exactly
  # once with O_EXCL|O_NOFOLLOW, then give curl only that inherited stdout
  # descriptor. A pathname replacement can make publication fail, but it can
  # neither redirect authenticated bytes nor make curl overwrite another file.
  if ! python3 - "$output" "$max_bytes" curl \
    ${curl_args[@]+"${curl_args[@]}"} \
    ${PROXY_ARGS[@]+"${PROXY_ARGS[@]}"} "$url" <<'PY'
import os, stat, subprocess, sys

output_path, maximum_raw, *command = sys.argv[1:]
maximum = int(maximum_raw)
flags = (
    os.O_WRONLY | os.O_CREAT | os.O_EXCL |
    getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0)
)
fd = os.open(output_path, flags, 0o600)
returncode = 1
try:
    before = os.fstat(fd)
    if not stat.S_ISREG(before.st_mode) or before.st_nlink != 1:
        raise SystemExit("download output descriptor is not one single-link regular file")
    try:
        completed = subprocess.run(command, check=False, stdout=fd, close_fds=True)
    except OSError as error:
        raise SystemExit(f"could not execute bounded curl transfer: {error}") from error
    os.fsync(fd)
    after = os.fstat(fd)
    named = os.stat(output_path, follow_symlinks=False)
    if ((before.st_dev, before.st_ino) != (after.st_dev, after.st_ino) or
            after.st_nlink != 1 or after.st_size > maximum or
            (after.st_dev, after.st_ino) != (named.st_dev, named.st_ino)):
        raise SystemExit("download output identity or byte bound changed during transfer")
    returncode = completed.returncode
finally:
    os.close(fd)
raise SystemExit(returncode)
PY
  then
    return 1
  fi
  verify_bounded_download_file "$output" "$max_bytes"
}

require_transfer_capacity() {
  local temporary_root="$1" destination_root="$2" archive_bytes="$3"
  local expanded_bytes="$4" label="$5" temporary_bytes destination_bytes
  [[ "$archive_bytes" =~ ^[0-9]+$ ]] && [[ "$expanded_bytes" =~ ^[0-9]+$ ]] || {
    err "Invalid transfer budget for $label"
    return 1
  }
  temporary_bytes=$((archive_bytes + expanded_bytes + INSTALLER_FREE_SPACE_HEADROOM_BYTES))
  destination_bytes=$((expanded_bytes + INSTALLER_FREE_SPACE_HEADROOM_BYTES))
  require_filesystem_capacity "$temporary_root" "$temporary_bytes" \
    "$label temporary workspace" || return 1
  require_filesystem_capacity "$destination_root" "$destination_bytes" \
    "$label destination" || return 1
}

check_disk_space() {
  # Exact package sizes are checked after authentication. This first fence
  # guarantees enough room for manifests and an atomic publication without
  # assuming that TMP and DEST share one filesystem.
  require_filesystem_capacity "$DEST" "$INSTALLER_FREE_SPACE_HEADROOM_BYTES" \
    "installation destination" || exit 1
}

process_family_manifest_metadata() {
  local manifest="$1"
  local family_kind="${2:-triplet}"
  python3 - "$manifest" "$family_kind" <<'PY'
import json, os, re, stat, sys

path, family_kind = sys.argv[1:]
flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
fd = os.open(path, flags)
try:
    before = os.fstat(fd)
    if not stat.S_ISREG(before.st_mode) or before.st_nlink != 1 or before.st_size > 4 * 1024 * 1024:
        raise SystemExit("unsafe component manifest")
    payload = b""
    while True:
        chunk = os.read(fd, 65536)
        if not chunk:
            break
        payload += chunk
    after = os.fstat(fd)
    if (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns) != (
        after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns
    ):
        raise SystemExit("component manifest changed while read")
finally:
    os.close(fd)
manifest = json.loads(payload)
identity = manifest["identity"]
keys = ("build_id", "source_revision", "version", "target", "profile", "feature_contract")
if set(identity) != set(keys) or any(not isinstance(identity[key], str) for key in keys):
    raise SystemExit("invalid component identity")
if not re.fullmatch(r"[0-9a-f]{64}", identity["build_id"]) or identity["build_id"] == "0" * 64:
    raise SystemExit("invalid or sentinel component build identity")
if not re.fullmatch(r"sha256:[0-9a-f]{64}", manifest.get("manifest_id", "")):
    raise SystemExit("invalid manifest identity")
executables = {
    (record.get("role"), record.get("component"), record.get("path"))
    for record in manifest.get("files", [])
    if record.get("kind") == "executable"
}
expected = {
    ("cli", "ft", "ft"),
    ("mux-server", "frankenterm-mux-server", "frankenterm-mux-server"),
    ("pty-guardian", "frankenterm-pty-guardian", "frankenterm-pty-guardian"),
}
if family_kind == "app":
    expected = {
        ("gui", "frankenterm-gui", "Contents/MacOS/frankenterm-gui"),
        ("cli", "ft", "Contents/MacOS/ft"),
        ("mux-server", "frankenterm-mux-server", "Contents/MacOS/frankenterm-mux-server"),
        ("pty-guardian", "frankenterm-pty-guardian", "Contents/MacOS/frankenterm-pty-guardian"),
    }
if executables != expected:
    raise SystemExit("component manifest has the wrong exact executable role inventory")
inventory = manifest.get("inventory")
if (not isinstance(inventory, dict) or
        not isinstance(inventory.get("total_bytes"), int) or
        isinstance(inventory.get("total_bytes"), bool) or
        inventory["total_bytes"] < 0 or inventory["total_bytes"] > 16 * 1024 * 1024 * 1024):
    raise SystemExit("component manifest has an invalid bounded byte inventory")
print("\t".join([
    manifest["manifest_id"], *(identity[key] for key in keys), str(inventory["total_bytes"]),
]))
PY
}

fsync_installer_tree() {
  local root="$1"
  python3 - "$root" <<'PY'
import os, stat, sys

root = os.path.abspath(sys.argv[1])
nofollow = getattr(os, "O_NOFOLLOW", 0)
directory = getattr(os, "O_DIRECTORY", 0)
if not nofollow or not directory:
    raise SystemExit("descriptor-relative nofollow fsync is unavailable")
root_fd = os.open(root, os.O_RDONLY | directory | nofollow | getattr(os, "O_CLOEXEC", 0))
entries = 0
def sync_dir(fd, depth=0):
    global entries
    if depth > 128:
        raise SystemExit("installer tree exceeds depth bound")
    names = []
    name_bytes = 0
    with os.scandir(fd) as iterator:
        for entry in iterator:
            encoded = entry.name.encode("utf-8", "surrogateescape")
            name_bytes += len(encoded)
            if entries + len(names) + 1 > 1_000_000 or name_bytes > 64 * 1024 * 1024:
                raise SystemExit("installer tree exceeds bounded enumeration budget")
            names.append(entry.name)
    names.sort(key=lambda value: value.encode("utf-8", "surrogateescape"))
    for name in names:
        entries += 1
        if entries > 1_000_000:
            raise SystemExit("installer tree exceeds entry bound")
        observed = os.stat(name, dir_fd=fd, follow_symlinks=False)
        if stat.S_ISLNK(observed.st_mode):
            continue
        if stat.S_ISDIR(observed.st_mode):
            child = os.open(name, os.O_RDONLY | directory | nofollow, dir_fd=fd)
            try:
                sync_dir(child, depth + 1)
            finally:
                os.close(child)
        elif stat.S_ISREG(observed.st_mode):
            child = os.open(name, os.O_RDONLY | nofollow, dir_fd=fd)
            try:
                os.fsync(child)
            finally:
                os.close(child)
        else:
            raise SystemExit("installer tree contains a special file")
    os.fsync(fd)
try:
    sync_dir(root_fd)
finally:
    os.close(root_fd)
parent_fd = os.open(os.path.dirname(root), os.O_RDONLY | directory | nofollow)
try:
    os.fsync(parent_fd)
finally:
    os.close(parent_fd)
PY
}

fsync_installer_directory() {
  python3 - "$1" <<'PY'
import os, stat, sys
path = sys.argv[1]
flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0)
fd = os.open(path, flags)
try:
    observed = os.fstat(fd)
    if not stat.S_ISDIR(observed.st_mode) or observed.st_uid != os.geteuid():
        raise SystemExit("installer directory fsync authority is unsafe")
    os.fsync(fd)
finally:
    os.close(fd)
PY
}

ensure_exact_staged_file() {
  local source="$1" target="$2" mode="$3"
  python3 - "$source" "$target" "$mode" <<'PY'
import os, stat, sys

source_path, target_path, requested_mode = sys.argv[1:]
final_mode = int(requested_mode, 8)
nofollow = getattr(os, "O_NOFOLLOW", 0)
directory = getattr(os, "O_DIRECTORY", 0)
cloexec = getattr(os, "O_CLOEXEC", 0)
if not nofollow or not directory:
    raise SystemExit("descriptor-relative nofollow staging is unavailable")

source_fd = os.open(source_path, os.O_RDONLY | nofollow | cloexec)
parent_path, target_name = os.path.split(os.path.abspath(target_path))
if not target_name or target_name in (".", "..") or "/" in target_name:
    raise SystemExit("staged target is not one canonical child")
parent_fd = os.open(parent_path, os.O_RDONLY | directory | nofollow | cloexec)
target_fd = -1
try:
    source_before = os.fstat(source_fd)
    parent_before = os.fstat(parent_fd)
    source_named_before = os.stat(source_path, follow_symlinks=False)
    parent_named_before = os.stat(parent_path, follow_symlinks=False)
    if (not stat.S_ISREG(source_before.st_mode) or source_before.st_nlink != 1 or
            source_before.st_size > 16 * 1024 * 1024 * 1024 or
            (source_before.st_dev, source_before.st_ino) !=
            (source_named_before.st_dev, source_named_before.st_ino)):
        raise SystemExit("staged source is not one bounded single-link regular file")
    if (not stat.S_ISDIR(parent_before.st_mode) or parent_before.st_uid != os.geteuid() or
            stat.S_IMODE(parent_before.st_mode) not in (0o700, 0o555) or
            (parent_before.st_dev, parent_before.st_ino) !=
            (parent_named_before.st_dev, parent_named_before.st_ino)):
        raise SystemExit("staged target parent is not one owner-controlled directory")

    try:
        target_fd = os.open(
            target_name,
            os.O_RDWR | os.O_CREAT | os.O_EXCL | nofollow | cloexec,
            0o600,
            dir_fd=parent_fd,
        )
        os.fsync(parent_fd)
    except FileExistsError:
        target_fd = os.open(target_name, os.O_RDONLY | nofollow | cloexec, dir_fd=parent_fd)

    target_before = os.fstat(target_fd)
    named_before = os.stat(target_name, dir_fd=parent_fd, follow_symlinks=False)
    target_mode = stat.S_IMODE(target_before.st_mode)
    if (not stat.S_ISREG(target_before.st_mode) or target_before.st_uid != os.geteuid() or
            target_before.st_nlink != 1 or target_before.st_dev != named_before.st_dev or
            target_before.st_ino != named_before.st_ino or target_before.st_size > source_before.st_size or
            target_mode not in (0o600, final_mode)):
        raise SystemExit("retained staged file is not one safe resumable regular file")

    remaining = target_before.st_size
    while remaining:
        width = min(1024 * 1024, remaining)
        source_chunk = os.read(source_fd, width)
        target_chunk = os.read(target_fd, width)
        if len(source_chunk) != width or target_chunk != source_chunk:
            raise SystemExit("retained staged file is not an exact source prefix")
        remaining -= width
    source_after_prefix = os.fstat(source_fd)
    target_after_prefix = os.fstat(target_fd)
    if ((source_before.st_dev, source_before.st_ino, source_before.st_size,
         source_before.st_mtime_ns, source_before.st_ctime_ns) !=
        (source_after_prefix.st_dev, source_after_prefix.st_ino, source_after_prefix.st_size,
         source_after_prefix.st_mtime_ns, source_after_prefix.st_ctime_ns) or
        (target_before.st_dev, target_before.st_ino, target_before.st_size,
         target_before.st_mtime_ns, target_before.st_ctime_ns) !=
        (target_after_prefix.st_dev, target_after_prefix.st_ino, target_after_prefix.st_size,
         target_after_prefix.st_mtime_ns, target_after_prefix.st_ctime_ns)):
        raise SystemExit("staged source or target changed during prefix validation")

    if target_before.st_size < source_before.st_size:
        os.fchmod(target_fd, 0o600)
        os.fsync(target_fd)
        os.close(target_fd)
        target_fd = os.open(target_name, os.O_RDWR | nofollow | cloexec, dir_fd=parent_fd)
        reopened = os.fstat(target_fd)
        if (reopened.st_dev, reopened.st_ino, reopened.st_size) != (
                target_before.st_dev, target_before.st_ino, target_before.st_size):
            raise SystemExit("staged target changed while reopening for prefix completion")
        os.lseek(source_fd, target_before.st_size, os.SEEK_SET)
        os.lseek(target_fd, target_before.st_size, os.SEEK_SET)
        remaining = source_before.st_size - target_before.st_size
        while remaining:
            chunk = os.read(source_fd, min(1024 * 1024, remaining))
            if not chunk:
                raise SystemExit("staged source truncated during prefix completion")
            view = memoryview(chunk)
            while view:
                written = os.write(target_fd, view)
                if written <= 0:
                    raise SystemExit("staged target write made no progress")
                view = view[written:]
            remaining -= len(chunk)
        os.fsync(target_fd)

    target_before_final_read = os.fstat(target_fd)
    os.lseek(source_fd, 0, os.SEEK_SET)
    os.lseek(target_fd, 0, os.SEEK_SET)
    remaining = source_before.st_size
    while remaining:
        width = min(1024 * 1024, remaining)
        source_chunk = os.read(source_fd, width)
        target_chunk = os.read(target_fd, width)
        if len(source_chunk) != width or target_chunk != source_chunk:
            raise SystemExit("completed staged file differs from its pinned source")
        remaining -= width
    if os.read(target_fd, 1):
        raise SystemExit("completed staged file has an unexpected suffix")
    source_final = os.fstat(source_fd)
    target_final = os.fstat(target_fd)
    named_final = os.stat(target_name, dir_fd=parent_fd, follow_symlinks=False)
    if ((source_before.st_dev, source_before.st_ino, source_before.st_size,
         source_before.st_mtime_ns, source_before.st_ctime_ns) !=
        (source_final.st_dev, source_final.st_ino, source_final.st_size,
         source_final.st_mtime_ns, source_final.st_ctime_ns) or
        (target_before_final_read.st_dev, target_before_final_read.st_ino,
         target_before_final_read.st_size, target_before_final_read.st_mtime_ns,
         target_before_final_read.st_ctime_ns) !=
        (target_final.st_dev, target_final.st_ino, target_final.st_size,
         target_final.st_mtime_ns, target_final.st_ctime_ns) or
        target_final.st_dev != named_final.st_dev or target_final.st_ino != named_final.st_ino or
        target_final.st_size != source_before.st_size or target_final.st_nlink != 1):
        raise SystemExit("staged file identity changed during exact completion")
    os.fchmod(target_fd, final_mode)
    os.fsync(target_fd)
    sealed = os.fstat(target_fd)
    named_sealed = os.stat(target_name, dir_fd=parent_fd, follow_symlinks=False)
    if (sealed.st_dev, sealed.st_ino, sealed.st_size, stat.S_IMODE(sealed.st_mode)) != (
            named_sealed.st_dev, named_sealed.st_ino, source_before.st_size, final_mode):
        raise SystemExit("staged file seal did not survive exact readback")
    os.fsync(parent_fd)
    parent_named_final = os.stat(parent_path, follow_symlinks=False)
    if (parent_before.st_dev, parent_before.st_ino) != (
            parent_named_final.st_dev, parent_named_final.st_ino):
        raise SystemExit("staged target parent detached during publication")
finally:
    if target_fd >= 0:
        os.close(target_fd)
    os.close(parent_fd)
    os.close(source_fd)
PY
}

ensure_exact_staged_tree() {
  local source="$1" target="$2"
  python3 - "$source" "$target" <<'PY'
import os, signal, stat, sys

source_path, target_path = map(os.path.abspath, sys.argv[1:])
nofollow = getattr(os, "O_NOFOLLOW", 0)
directory = getattr(os, "O_DIRECTORY", 0)
cloexec = getattr(os, "O_CLOEXEC", 0)
if not nofollow or not directory:
    raise SystemExit("descriptor-relative nofollow tree staging is unavailable")

MAX_ENTRIES = 1_000_000
MAX_NAME_BYTES = 64 * 1024 * 1024
MAX_BYTES = 16 * 1024 * 1024 * 1024
MAX_DEPTH = 128
budget = {
    "source_entries": 0,
    "source_name_bytes": 0,
    "target_entries": 0,
    "target_name_bytes": 0,
    "bytes": 0,
    "files": 0,
}
fail_after = 0
if os.environ.get("FT_INSTALL_TEST_ENABLE_FAILPOINTS") == "1":
    fail_after = int(os.environ.get("FT_INSTALL_TEST_STAGE_FAIL_AFTER_FILES", "0"))

def stable(metadata):
    return (metadata.st_dev, metadata.st_ino, metadata.st_mode, metadata.st_size,
            metadata.st_mtime_ns, metadata.st_ctime_ns)

def scan_names(fd, kind):
    names = []
    with os.scandir(fd) as entries:
        for entry in entries:
            encoded = entry.name.encode("utf-8", "surrogateescape")
            budget[f"{kind}_entries"] += 1
            budget[f"{kind}_name_bytes"] += len(encoded)
            if (budget[f"{kind}_entries"] > MAX_ENTRIES or
                    budget[f"{kind}_name_bytes"] > MAX_NAME_BYTES):
                raise SystemExit(f"{kind} app tree exceeds its bounded inventory")
            names.append(entry.name)
    names.sort(key=lambda value: value.encode("utf-8", "surrogateescape"))
    return names

def exact_file(source_dir_fd, target_dir_fd, name, source_named):
    source_fd = os.open(name, os.O_RDONLY | nofollow | cloexec, dir_fd=source_dir_fd)
    target_fd = -1
    try:
        source_before = os.fstat(source_fd)
        if (not stat.S_ISREG(source_before.st_mode) or source_before.st_nlink != 1 or
                stable(source_before) != stable(source_named)):
            raise SystemExit("app source file changed before materialization")
        budget["bytes"] += source_before.st_size
        if budget["bytes"] > MAX_BYTES:
            raise SystemExit("app tree exceeds its bounded byte inventory")
        desired_mode = 0o555 if source_before.st_mode & 0o111 else 0o444
        try:
            target_fd = os.open(
                name,
                os.O_RDWR | os.O_CREAT | os.O_EXCL | nofollow | cloexec,
                0o600,
                dir_fd=target_dir_fd,
            )
            os.fsync(target_dir_fd)
        except FileExistsError:
            target_fd = os.open(name, os.O_RDONLY | nofollow | cloexec, dir_fd=target_dir_fd)
        target_before = os.fstat(target_fd)
        target_named = os.stat(name, dir_fd=target_dir_fd, follow_symlinks=False)
        target_mode = stat.S_IMODE(target_before.st_mode)
        if (not stat.S_ISREG(target_before.st_mode) or target_before.st_uid != os.geteuid() or
                target_before.st_nlink != 1 or target_before.st_dev != target_named.st_dev or
                target_before.st_ino != target_named.st_ino or
                target_before.st_size > source_before.st_size or target_mode & 0o7022):
            raise SystemExit("retained app file is not a safe resumable regular file")
        remaining = target_before.st_size
        while remaining:
            width = min(1024 * 1024, remaining)
            source_chunk = os.read(source_fd, width)
            target_chunk = os.read(target_fd, width)
            if len(source_chunk) != width or target_chunk != source_chunk:
                raise SystemExit("retained app file is not an exact source prefix")
            remaining -= width
        source_after_prefix = os.fstat(source_fd)
        target_after_prefix = os.fstat(target_fd)
        if stable(source_before) != stable(source_after_prefix) or stable(target_before) != stable(target_after_prefix):
            raise SystemExit("app source or target changed during prefix validation")
        if target_before.st_size < source_before.st_size:
            os.fchmod(target_fd, 0o600)
            os.fsync(target_fd)
            os.close(target_fd)
            target_fd = os.open(name, os.O_RDWR | nofollow | cloexec, dir_fd=target_dir_fd)
            reopened = os.fstat(target_fd)
            if (reopened.st_dev, reopened.st_ino, reopened.st_size) != (
                    target_before.st_dev, target_before.st_ino, target_before.st_size):
                raise SystemExit("retained app file changed while reopening")
            os.lseek(source_fd, target_before.st_size, os.SEEK_SET)
            os.lseek(target_fd, target_before.st_size, os.SEEK_SET)
            remaining = source_before.st_size - target_before.st_size
            while remaining:
                chunk = os.read(source_fd, min(1024 * 1024, remaining))
                if not chunk:
                    raise SystemExit("app source truncated during prefix completion")
                view = memoryview(chunk)
                while view:
                    written = os.write(target_fd, view)
                    if written <= 0:
                        raise SystemExit("app stage write made no progress")
                    view = view[written:]
                remaining -= len(chunk)
            os.fsync(target_fd)
        target_before_final_read = os.fstat(target_fd)
        os.lseek(source_fd, 0, os.SEEK_SET)
        os.lseek(target_fd, 0, os.SEEK_SET)
        remaining = source_before.st_size
        while remaining:
            width = min(1024 * 1024, remaining)
            source_chunk = os.read(source_fd, width)
            target_chunk = os.read(target_fd, width)
            if len(source_chunk) != width or target_chunk != source_chunk:
                raise SystemExit("completed app file differs from its pinned source")
            remaining -= width
        if os.read(target_fd, 1):
            raise SystemExit("completed app file has an unexpected suffix")
        source_final = os.fstat(source_fd)
        target_final = os.fstat(target_fd)
        named_final = os.stat(name, dir_fd=target_dir_fd, follow_symlinks=False)
        if (stable(source_before) != stable(source_final) or
                stable(target_before_final_read) != stable(target_final) or
                target_final.st_dev != named_final.st_dev or target_final.st_ino != named_final.st_ino or
                target_final.st_size != source_before.st_size or target_final.st_nlink != 1):
            raise SystemExit("app file identity changed during exact completion")
        os.fchmod(target_fd, desired_mode)
        os.fsync(target_fd)
        budget["files"] += 1
        if fail_after and budget["files"] == fail_after:
            os.kill(os.getppid(), signal.SIGKILL)
            os.kill(os.getpid(), signal.SIGKILL)
    finally:
        if target_fd >= 0:
            os.close(target_fd)
        os.close(source_fd)

def materialize(source_fd, target_fd, depth):
    if depth > MAX_DEPTH:
        raise SystemExit("app tree exceeds its bounded depth")
    source_before = os.fstat(source_fd)
    target_before = os.fstat(target_fd)
    if (not stat.S_ISDIR(source_before.st_mode) or not stat.S_ISDIR(target_before.st_mode) or
            target_before.st_uid != os.geteuid() or stat.S_IMODE(target_before.st_mode) & 0o7022):
        raise SystemExit("app tree contains an unsafe directory")
    os.fchmod(target_fd, 0o700)
    os.fsync(target_fd)
    source_names = scan_names(source_fd, "source")
    target_names = scan_names(target_fd, "target")
    unexpected = set(target_names).difference(source_names)
    if unexpected:
        raise SystemExit("retained app stage contains an unexpected entry")
    target_set = set(target_names)
    for name in source_names:
        source_named = os.stat(name, dir_fd=source_fd, follow_symlinks=False)
        if stat.S_ISREG(source_named.st_mode):
            exact_file(source_fd, target_fd, name, source_named)
        elif stat.S_ISDIR(source_named.st_mode):
            if name not in target_set:
                os.mkdir(name, 0o700, dir_fd=target_fd)
                os.fsync(target_fd)
            target_named = os.stat(name, dir_fd=target_fd, follow_symlinks=False)
            if not stat.S_ISDIR(target_named.st_mode) or target_named.st_uid != os.geteuid():
                raise SystemExit("retained app directory changed type or owner")
            source_child = os.open(name, os.O_RDONLY | directory | nofollow | cloexec, dir_fd=source_fd)
            target_child = os.open(name, os.O_RDONLY | directory | nofollow | cloexec, dir_fd=target_fd)
            try:
                materialize(source_child, target_child, depth + 1)
            finally:
                os.close(target_child)
                os.close(source_child)
        elif stat.S_ISLNK(source_named.st_mode):
            link_target = os.readlink(name, dir_fd=source_fd)
            if len(os.fsencode(link_target)) > 4096:
                raise SystemExit("app source symlink target exceeds its bound")
            if name in target_set:
                target_named = os.stat(name, dir_fd=target_fd, follow_symlinks=False)
                if (not stat.S_ISLNK(target_named.st_mode) or
                        target_named.st_uid != os.geteuid() or
                        os.readlink(name, dir_fd=target_fd) != link_target):
                    raise SystemExit("retained app symlink differs from its source")
            else:
                os.symlink(link_target, name, dir_fd=target_fd)
                os.fsync(target_fd)
            if stable(source_named) != stable(os.stat(name, dir_fd=source_fd, follow_symlinks=False)):
                raise SystemExit("app source symlink changed while read")
        else:
            raise SystemExit("app source tree contains a special file")
    source_after = os.fstat(source_fd)
    if stable(source_before) != stable(source_after):
        raise SystemExit("app source directory changed during materialization")
    os.fchmod(target_fd, 0o555)
    os.fsync(target_fd)

source_fd = os.open(source_path, os.O_RDONLY | directory | nofollow | cloexec)
parent_path, target_name = os.path.split(target_path)
if not target_name or target_name in (".", "..") or "/" in target_name:
    raise SystemExit("app stage target is not one canonical child")
parent_fd = os.open(parent_path, os.O_RDONLY | directory | nofollow | cloexec)
target_fd = -1
try:
    source_metadata = os.fstat(source_fd)
    parent_metadata = os.fstat(parent_fd)
    source_named = os.stat(source_path, follow_symlinks=False)
    parent_named = os.stat(parent_path, follow_symlinks=False)
    if (not stat.S_ISDIR(source_metadata.st_mode) or
            (source_metadata.st_dev, source_metadata.st_ino) !=
            (source_named.st_dev, source_named.st_ino)):
        raise SystemExit("app source root is not one stable nofollow directory")
    if (not stat.S_ISDIR(parent_metadata.st_mode) or parent_metadata.st_uid != os.geteuid() or
            stat.S_IMODE(parent_metadata.st_mode) & 0o7022 or
            (parent_metadata.st_dev, parent_metadata.st_ino) !=
            (parent_named.st_dev, parent_named.st_ino)):
        raise SystemExit("app stage parent is not one private owner-controlled directory")
    try:
        os.mkdir(target_name, 0o700, dir_fd=parent_fd)
        os.fsync(parent_fd)
    except FileExistsError:
        pass
    target_fd = os.open(target_name, os.O_RDONLY | directory | nofollow | cloexec, dir_fd=parent_fd)
    target_named = os.stat(target_name, dir_fd=parent_fd, follow_symlinks=False)
    target_opened = os.fstat(target_fd)
    if (not stat.S_ISDIR(target_named.st_mode) or target_named.st_uid != os.geteuid() or
            target_named.st_dev != target_opened.st_dev or target_named.st_ino != target_opened.st_ino):
        raise SystemExit("app stage root is not one stable owner-controlled directory")
    materialize(source_fd, target_fd, 0)
    os.fsync(parent_fd)
    parent_named_final = os.stat(parent_path, follow_symlinks=False)
    if (parent_metadata.st_dev, parent_metadata.st_ino) != (
            parent_named_final.st_dev, parent_named_final.st_ino):
        raise SystemExit("app stage parent detached during materialization")
finally:
    if target_fd >= 0:
        os.close(target_fd)
    os.close(parent_fd)
    os.close(source_fd)
PY
}

validate_installer_stage_inventory() {
  local root="$1" kind="$2"
  python3 - "$root" "$kind" <<'PY'
import os, stat, sys

root, kind = sys.argv[1:]
expected = {
    "generation": {
        "ft", "frankenterm-mux-server", "frankenterm-pty-guardian",
        "verify-components.sh", "process-family.component-manifest.json",
    },
    "legacy": {
        "ft", "frankenterm-mux-server", "frankenterm-pty-guardian",
        "legacy-family.json",
    },
}[kind]
fd = os.open(root, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) |
             getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0))
try:
    names = os.listdir(fd)
    if len(names) > len(expected) or not set(names).issubset(expected):
        raise SystemExit("retained installer stage has an unexpected inventory")
    for name in names:
        observed = os.stat(name, dir_fd=fd, follow_symlinks=False)
        if not stat.S_ISREG(observed.st_mode) or observed.st_nlink != 1:
            raise SystemExit("retained installer stage contains an unsafe entry")
finally:
    os.close(fd)
PY
}

installer_stage_mode() {
  python3 - "$1" <<'PY'
import os, stat, sys
observed = os.lstat(sys.argv[1])
if not stat.S_ISDIR(observed.st_mode) or stat.S_ISLNK(observed.st_mode) or observed.st_uid != os.geteuid():
    raise SystemExit("installer stage is not one owner-controlled directory")
print(f"{stat.S_IMODE(observed.st_mode):04o}")
PY
}

atomic_transition_txid() {
  python3 - "$1" <<'PY'
import hashlib, sys
print(hashlib.sha256(("frankenterm.install.atomic-transition.v5\0" + sys.argv[1]).encode()).hexdigest()[:32])
PY
}

atomic_path_content_id() {
  local helper="$1" parent="$2" name="$3" output prefix
  output=$("$helper" setup __atomic-path-content-id --parent "$parent" --name "$name") || return 1
  prefix="FT_ATOMIC_PATH_CONTENT_ID_V3="
  [[ "$output" == "$prefix"* ]] && [[ "$output" != *$'\n'* ]] || return 1
  output="${output#"$prefix"}"
  [[ "$output" =~ ^sha256:[0-9a-f]{64}$ ]] || return 1
  printf '%s\n' "$output"
}

atomic_path_transition() {
  local helper="$1" parent="$2" stage="$3" target="$4" txid="$5"
  local stage_id="$6" target_id="$7" operation="$8" output prefix
  output=$("$helper" setup __atomic-path-transition \
    --parent "$parent" --stage-name "$stage" --target-name "$target" \
    --transaction-id "$txid" --stage-content-id "$stage_id" \
    --target-content-id "$target_id" --operation "$operation") || return 1
  prefix="FT_ATOMIC_PATH_TRANSITION_V5=${txid}:${operation}:${stage}:${target}:"
  [ "$output" = "${prefix}applied" ] || [ "$output" = "${prefix}already-applied" ]
}

installer_failpoint() {
  local point="$1"
  if [ "${FT_INSTALL_TEST_ENABLE_FAILPOINTS:-0}" = 1 ] && \
     [ "${FT_INSTALL_TEST_FAILPOINT:-}" = "$point" ]; then
    kill -KILL "$$"
  fi
}

legacy_process_family_manifest() {
  local root="$1" output="$2"
  python3 - "$root" "$output" <<'PY'
import hashlib, json, os, re, stat, sys

root, output = sys.argv[1:]
roles = ("ft", "frankenterm-mux-server", "frankenterm-pty-guardian")
marker = re.compile(rb"FT_ATOMIC_COMPONENT_IDENTITY_V1:([0-9a-f]{64}):([A-Za-z0-9._+-]+):([A-Za-z0-9._+-]+):([A-Za-z0-9._+-]+):([A-Za-z0-9._+-]+);")
records, identity = [], None
for role in roles:
    path = os.path.join(root, role)
    fd = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0))
    try:
        before = os.fstat(fd)
        if not stat.S_ISREG(before.st_mode) or before.st_nlink != 1 or before.st_size > 512 * 1024 * 1024:
            raise SystemExit("unsafe legacy process-family member")
        digest = hashlib.sha256()
        length = 0
        carry = b""
        prefix_count = 0
        found = []
        while True:
            chunk = os.read(fd, 1024 * 1024)
            if not chunk:
                break
            previous_length = length
            length += len(chunk)
            if length > 512 * 1024 * 1024:
                raise SystemExit("legacy process-family member exceeds component cap")
            digest.update(chunk)
            payload = carry + chunk
            payload_offset = previous_length - len(carry)
            offset = 0
            while True:
                offset = payload.find(b"FT_ATOMIC_COMPONENT_IDENTITY_V1:", offset)
                if offset < 0:
                    break
                if payload_offset + offset + len(b"FT_ATOMIC_COMPONENT_IDENTITY_V1:") > previous_length:
                    prefix_count += 1
                    if prefix_count > 1:
                        raise SystemExit("legacy component contains duplicate raw atomic markers")
                offset += 1
            for match in marker.finditer(payload):
                if payload_offset + match.end() > previous_length:
                    found.append(match.groups())
                    if len(found) > 1:
                        raise SystemExit("legacy component contains duplicate valid atomic markers")
            carry = payload[-2048:]
        after = os.fstat(fd)
        if (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns) != (
            after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns
        ):
            raise SystemExit("legacy process-family member changed while read")
    finally:
        os.close(fd)
    if prefix_count != 1 or len(found) != 1:
        raise SystemExit("legacy component must contain exactly one raw atomic marker")
    build, found_role, target, profile, version = (item.decode() for item in found[0])
    if build == "0" * 64:
        raise SystemExit("legacy component uses the forbidden all-zero build identity")
    if found_role != role:
        raise SystemExit("legacy component marker role mismatch")
    normalized = (build, target, profile, version)
    if identity is None:
        identity = normalized
    elif identity != normalized:
        raise SystemExit("legacy process family is mixed")
    records.append({"bytes": length, "path": role, "sha256": digest.hexdigest()})
manifest = {
    "files": records,
    "identity": {"build_id": identity[0], "target": identity[1], "profile": identity[2], "version": identity[3]},
    "schema_version": "frankenterm.legacy-process-family.v1",
}
encoded = json.dumps(manifest, sort_keys=True, separators=(",", ":")).encode()
manifest["manifest_id"] = "sha256:" + hashlib.sha256(encoded).hexdigest()
payload = (json.dumps(manifest, sort_keys=True, separators=(",", ":")) + "\n").encode()
if output != "-":
    fd = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o444)
    try:
        os.write(fd, payload)
        os.fsync(fd)
    finally:
        os.close(fd)
print(manifest["manifest_id"])
PY
}

verify_canonical_generation() {
  local generation="$1" expected_version="${2:-}" verifier_authority="${3:-}"
  local metadata manifest_id build_id source_revision version target profile feature_contract inventory_bytes manifest
  manifest="$generation/process-family.component-manifest.json"
  [ -f "$verifier_authority" ] && [ ! -L "$verifier_authority" ] || return 1
  [ -f "$manifest" ] && [ ! -L "$manifest" ] || return 1
  bash "$verifier_authority" verify --root "$generation" --manifest "$manifest" >/dev/null || return 1
  metadata=$(process_family_manifest_metadata "$manifest" triplet) || return 1
  IFS=$'\t' read -r manifest_id build_id source_revision version target profile feature_contract inventory_bytes <<<"$metadata"
  [[ "$inventory_bytes" =~ ^[0-9]+$ ]] || return 1
  [ "$(basename "$generation")" = "${manifest_id#sha256:}" ] || return 1
  if [ -n "$expected_version" ]; then
    [ "$version" = "${expected_version#v}" ] || [ "$version" = "$expected_version" ] || return 1
  fi
  "$generation/ft" --version >/dev/null 2>&1 || return 1
  "$generation/frankenterm-mux-server" --version >/dev/null 2>&1 || return 1
  "$generation/frankenterm-pty-guardian" --version >/dev/null 2>&1 || return 1
}

stable_entrypoint_is_managed() {
  local name="$1"
  [ -L "$DEST/$name" ] && \
    [ "$(readlink "$DEST/$name")" = ".frankenterm-process-family/current/$name" ]
}

ensure_installer_process_family_root() {
  python3 - "$DEST" <<'PY'
import os, stat, sys

destination = os.path.abspath(sys.argv[1])
nofollow = getattr(os, "O_NOFOLLOW", 0)
directory = getattr(os, "O_DIRECTORY", 0)
cloexec = getattr(os, "O_CLOEXEC", 0)
if not nofollow or not directory:
    raise SystemExit("descriptor-relative nofollow installation is unavailable")

def stable(metadata):
    return (metadata.st_dev, metadata.st_ino, metadata.st_mode,
            metadata.st_mtime_ns, metadata.st_ctime_ns)

def require_private_directory(metadata, label):
    if (not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != os.geteuid() or
            stat.S_IMODE(metadata.st_mode) & 0o022):
        raise SystemExit(f"{label} is not one private owner-controlled directory")

def open_or_create(parent_fd, name, label):
    try:
        os.mkdir(name, 0o700, dir_fd=parent_fd)
        os.fsync(parent_fd)
    except FileExistsError:
        pass
    named = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
    fd = os.open(name, os.O_RDONLY | directory | nofollow | cloexec, dir_fd=parent_fd)
    opened = os.fstat(fd)
    require_private_directory(opened, label)
    if (opened.st_dev, opened.st_ino) != (named.st_dev, named.st_ino):
        os.close(fd)
        raise SystemExit(f"{label} changed while it was opened")
    os.fchmod(fd, 0o700)
    os.fsync(fd)
    return fd, stable(opened)

destination_named = os.stat(destination, follow_symlinks=False)
destination_fd = os.open(destination, os.O_RDONLY | directory | nofollow | cloexec)
managed_fd = generations_fd = -1
try:
    destination_opened = os.fstat(destination_fd)
    require_private_directory(destination_opened, "installer destination")
    if (destination_opened.st_dev, destination_opened.st_ino) != (
            destination_named.st_dev, destination_named.st_ino):
        raise SystemExit("installer destination changed while it was opened")
    managed_fd, managed_before = open_or_create(
        destination_fd, ".frankenterm-process-family", "managed process-family root")
    generations_fd, generations_before = open_or_create(
        managed_fd, "generations", "managed generations root")
    if os.fstat(generations_fd).st_dev != os.fstat(managed_fd).st_dev:
        raise SystemExit("managed process-family roots are not on one filesystem")
    os.fsync(generations_fd)
    os.fsync(managed_fd)
    os.fsync(destination_fd)
    destination_final = os.stat(destination, follow_symlinks=False)
    managed_final = os.stat(
        ".frankenterm-process-family", dir_fd=destination_fd, follow_symlinks=False)
    generations_final = os.stat("generations", dir_fd=managed_fd, follow_symlinks=False)
    if (destination_opened.st_dev, destination_opened.st_ino,
        destination_opened.st_uid, stat.S_IMODE(destination_opened.st_mode)) != (
        destination_final.st_dev, destination_final.st_ino,
        destination_final.st_uid, stat.S_IMODE(destination_final.st_mode)):
        raise SystemExit("installer destination changed while roots were prepared")
    if managed_before[:2] != (managed_final.st_dev, managed_final.st_ino):
        raise SystemExit("managed process-family root detached while prepared")
    if generations_before[:2] != (generations_final.st_dev, generations_final.st_ino):
        raise SystemExit("managed generations root detached while prepared")
finally:
    if generations_fd >= 0:
        os.close(generations_fd)
    if managed_fd >= 0:
        os.close(managed_fd)
    os.close(destination_fd)
PY
}

inspect_installer_process_family_authority() {
  python3 - "$DEST" <<'PY'
import os, re, stat, sys

destination = os.path.abspath(sys.argv[1])
nofollow = getattr(os, "O_NOFOLLOW", 0)
directory = getattr(os, "O_DIRECTORY", 0)
cloexec = getattr(os, "O_CLOEXEC", 0)
if not nofollow or not directory:
    raise SystemExit("descriptor-relative nofollow inspection is unavailable")

roles = ("ft", "frankenterm-mux-server", "frankenterm-pty-guardian")
managed_link = {
    role: f".frankenterm-process-family/current/{role}" for role in roles
}
current_pattern = re.compile(r"generations/(?:[0-9a-f]{64}|legacy-[0-9a-f]{64})\Z")

def stable(metadata):
    return (metadata.st_dev, metadata.st_ino, metadata.st_mode, metadata.st_size,
            metadata.st_mtime_ns, metadata.st_ctime_ns)

def open_private_directory(path=None, *, parent_fd=None, name=None, label):
    if path is not None:
        named = os.stat(path, follow_symlinks=False)
        fd = os.open(path, os.O_RDONLY | directory | nofollow | cloexec)
    else:
        named = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
        fd = os.open(name, os.O_RDONLY | directory | nofollow | cloexec, dir_fd=parent_fd)
    opened = os.fstat(fd)
    if (not stat.S_ISDIR(opened.st_mode) or opened.st_uid != os.geteuid() or
            stat.S_IMODE(opened.st_mode) & 0o022 or
            (opened.st_dev, opened.st_ino) != (named.st_dev, named.st_ino)):
        os.close(fd)
        raise SystemExit(f"{label} is not one stable private owner-controlled directory")
    return fd, stable(opened)

def child_snapshot(parent_fd, name, expected_link=None):
    try:
        observed = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
    except FileNotFoundError:
        return ("missing",)
    identity = stable(observed)
    if stat.S_ISLNK(observed.st_mode):
        target = os.readlink(name, dir_fd=parent_fd)
        if ((expected_link is not None and target != expected_link) or
                observed.st_uid != os.geteuid()):
            raise SystemExit(f"unsafe or unmanaged symlink at {name}")
        return ("managed", target, *identity)
    if stat.S_ISREG(observed.st_mode):
        if (expected_link is not None and
                (observed.st_uid != os.geteuid() or observed.st_nlink != 1 or
                 stat.S_IMODE(observed.st_mode) & 0o022 or
                 not stat.S_IMODE(observed.st_mode) & 0o111 or
                 observed.st_size > 512 * 1024 * 1024)):
            raise SystemExit(f"unsafe direct process-family entrypoint at {name}")
        return ("direct", *identity)
    raise SystemExit(f"unsafe process-family namespace entry at {name}")

destination_fd, destination_before = open_private_directory(
    destination, label="installer destination")
managed_fd = generations_fd = selected_fd = -1
try:
    managed_fd, managed_before = open_private_directory(
        parent_fd=destination_fd, name=".frankenterm-process-family",
        label="managed process-family root")
    generations_fd, generations_before = open_private_directory(
        parent_fd=managed_fd, name="generations", label="managed generations root")
    if os.fstat(generations_fd).st_dev != os.fstat(managed_fd).st_dev:
        raise SystemExit("managed process-family roots are not on one filesystem")

    current = child_snapshot(managed_fd, "current")
    current_target = ""
    if current[0] == "managed":
        current_target = current[1]
        if not current_pattern.fullmatch(current_target):
            raise SystemExit("current selector has a non-canonical target")
        generation_name = current_target[len("generations/"):]
        selected_fd, _ = open_private_directory(
            parent_fd=generations_fd, name=generation_name,
            label="selected process-family generation")
    elif current[0] != "missing":
        raise SystemExit("current selector is not an exact managed symlink")

    entries = tuple(
        child_snapshot(destination_fd, role, managed_link[role]) for role in roles
    )
    kinds = tuple(entry[0] for entry in entries)
    if current_target:
        if kinds != ("managed", "managed", "managed"):
            raise SystemExit("selected process family lacks three exact managed entrypoints")
        result = f"managed\t{current_target}"
    elif all(kind in ("missing", "managed") for kind in kinds):
        result = "initial"
    elif kinds == ("direct", "direct", "direct"):
        result = "legacy"
    else:
        raise SystemExit("incomplete or mixed process-family authority")

    if stable(os.fstat(destination_fd)) != destination_before:
        raise SystemExit("installer destination changed during authority inspection")
    if stable(os.fstat(managed_fd)) != managed_before:
        raise SystemExit("managed process-family root changed during authority inspection")
    if stable(os.fstat(generations_fd)) != generations_before:
        raise SystemExit("managed generations root changed during authority inspection")
    if child_snapshot(managed_fd, "current") != current:
        raise SystemExit("current selector changed during authority inspection")
    final_entries = tuple(
        child_snapshot(destination_fd, role, managed_link[role]) for role in roles
    )
    if final_entries != entries:
        raise SystemExit("stable process-family entrypoints changed during inspection")
    print(result)
finally:
    if selected_fd >= 0:
        os.close(selected_fd)
    if generations_fd >= 0:
        os.close(generations_fd)
    if managed_fd >= 0:
        os.close(managed_fd)
    os.close(destination_fd)
PY
}

installer_mux_ownership_state() {
  if [ "${FT_INSTALL_TEST_LIBRARY_ONLY:-0}" = 1 ] && \
     [ -n "${FT_INSTALL_TEST_MUX_OWNERSHIP_STATE:-}" ]; then
    case "$FT_INSTALL_TEST_MUX_OWNERSHIP_STATE" in
      active|inactive|ambiguous) printf '%s\n' "$FT_INSTALL_TEST_MUX_OWNERSHIP_STATE" ;;
      *) return 2 ;;
    esac
    return
  fi
  python3 - <<'PY'
import os, pathlib, subprocess, sys

names = {
    "frankenterm-mux-server",
    "wezterm-mux-server",
    "frankenterm-gui",
    "wezterm-gui",
}
truncated = {name[:15] for name in names}

def classify(command):
    if command.endswith(" (deleted)"):
        command = command[:-len(" (deleted)")]
    basename = os.path.basename(command)
    if basename in names:
        return "active"
    if basename in truncated:
        return "ambiguous"
    return "inactive"

if sys.platform.startswith("linux"):
    proc = pathlib.Path("/proc")
    if not proc.is_dir():
        print("ambiguous")
        raise SystemExit(0)
    ambiguous = False
    for process in proc.iterdir():
        if not process.name.isdigit():
            continue
        try:
            if process.stat().st_uid != os.geteuid():
                continue
            comm = (process / "comm").read_text(errors="surrogateescape").strip()
        except (FileNotFoundError, ProcessLookupError):
            continue
        except (OSError, UnicodeError):
            ambiguous = True
            continue
        if comm not in names and comm not in truncated:
            continue
        try:
            outcome = classify(os.readlink(process / "exe"))
        except (FileNotFoundError, ProcessLookupError):
            continue
        except OSError:
            ambiguous = True
            continue
        if outcome == "active":
            print("active")
            raise SystemExit(0)
        ambiguous = True
    print("ambiguous" if ambiguous else "inactive")
    raise SystemExit(0)

try:
    observed = subprocess.run(
        ["ps", "-U", str(os.geteuid()), "-o", "pid=", "-o", "comm="],
        check=False, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        text=True, timeout=10,
    )
except (OSError, subprocess.SubprocessError):
    print("ambiguous")
    raise SystemExit(0)
if observed.returncode != 0:
    print("ambiguous")
    raise SystemExit(0)
ambiguous = False
for line in observed.stdout.splitlines():
    fields = line.strip().split(maxsplit=1)
    if len(fields) != 2 or not fields[0].isdigit():
        if line.strip():
            ambiguous = True
        continue
    outcome = classify(fields[1])
    if outcome == "active":
        print("active")
        raise SystemExit(0)
    ambiguous = ambiguous or outcome == "ambiguous"
print("ambiguous" if ambiguous else "inactive")
PY
}

require_no_live_mux_for_initial_selector() {
  local state
  INITIAL_SELECTOR_HOLD_REASON=""
  state=$(installer_mux_ownership_state) || state=ambiguous
  case "$state" in
    inactive)
      INITIAL_SELECTOR_HOLD_REASON="inactive-census-without-shared-launcher-lease"
      warn "Mux census is inactive, but no launcher-shared lease proves that state remains inactive"
      ;;
    active)
      INITIAL_SELECTOR_HOLD_REASON="active-mux-owns-session-state"
      err "A live FrankenTerm/WezTerm mux owns session state; refusing initial selector creation"
      ;;
    *)
      INITIAL_SELECTOR_HOLD_REASON="ambiguous-mux-ownership"
      err "Mux ownership could not be proven inactive; refusing initial selector creation"
      ;;
  esac
  # A process census is evidence, not an exclusion primitive. Until every GUI,
  # CLI, and mux launcher participates in one lease, this function must never
  # authorize selector publication, including after an inactive observation.
  return 1
}

ensure_staged_symlink() {
  local target="$1" path="$2"
  if [ -L "$path" ]; then
    [ "$(readlink "$path")" = "$target" ]
  elif [ -e "$path" ]; then
    return 1
  else
    ln -s "$target" "$path"
  fi
}

publish_stable_entrypoint() {
  local helper="$1" name="$2" mode="$3" selected_generation="$4"
  local txid stage stage_id target_id operation
  if stable_entrypoint_is_managed "$name"; then
    if [ "$mode" = missing ] && [ ! -e "$DEST/.frankenterm-process-family/current" ] && \
       [ ! -L "$DEST/.frankenterm-process-family/current" ]; then
      # A first-install retry may encounter an exact managed link that is
      # deliberately still dangling. It remains non-executable until the one
      # selector publication activates the complete triplet.
      return 0
    fi
    cmp "$DEST/$name" "$selected_generation/$name" >/dev/null 2>&1 || return 1
    return 0
  fi
  txid=$(atomic_transition_txid "entrypoint:$DEST:$name:$selected_generation") || return 1
  stage=".ft-entrypoint-${name}-${txid}"
  ensure_staged_symlink ".frankenterm-process-family/current/$name" "$DEST/$stage" || return 1
  stage_id=$(atomic_path_content_id "$helper" "$DEST" "$stage") || return 1
  if [ "$mode" = missing ]; then
    target_id=missing
    operation=publish-noreplace
  else
    [ -f "$DEST/$name" ] && [ ! -L "$DEST/$name" ] || return 1
    cmp "$DEST/$name" "$selected_generation/$name" >/dev/null 2>&1 || return 1
    target_id=$(atomic_path_content_id "$helper" "$DEST" "$name") || return 1
    operation=exchange
  fi
  atomic_path_transition "$helper" "$DEST" "$stage" "$name" "$txid" \
    "$stage_id" "$target_id" "$operation"
}

install_process_family() {
  local ft_source="$1" mux_source="$2" guardian_source="$3"
  local manifest_source="$4" verifier_source="$5"
  local managed="$DEST/.frankenterm-process-family"
  local generations="$DEST/.frankenterm-process-family/generations"
  local metadata manifest_id build_id source_revision version target profile feature_contract inventory_bytes
  local generation_id generation stage stage_name helper="$ft_source" stage_id txid

  PENDING_PROCESS_FAMILY_GENERATION=""
  PUBLISHED_PROCESS_FAMILY_VERSION=""
  PUBLISHED_PROCESS_FAMILY_ROOT=""
  PUBLISHED_PROCESS_FAMILY_VERIFIER_AUTHORITY=""
  PROCESS_FAMILY_ACTIVATION_STATE=""
  PROCESS_FAMILY_ACTIVE_AUTHORITY=""
  PROCESS_FAMILY_ACTIVE_ROOT=""
  PROCESS_FAMILY_PENDING_REASON=""
  INITIAL_SELECTOR_HOLD_REASON=""

  command -v python3 >/dev/null 2>&1 || {
    err "python3 is required for crash-atomic installation"
    return 1
  }
  for source in "$ft_source" "$mux_source" "$guardian_source" "$manifest_source" "$verifier_source"; do
    [ -f "$source" ] && [ ! -L "$source" ] || {
      err "Unsafe process-family source: $source"
      return 1
    }
  done
  bash "$verifier_source" verify --root "$(dirname "$manifest_source")" \
    --manifest "$manifest_source" >/dev/null || {
      err "Atomic source family failed verification"
      return 1
    }
  metadata=$(process_family_manifest_metadata "$manifest_source" triplet) || return 1
  IFS=$'\t' read -r manifest_id build_id source_revision version target profile feature_contract inventory_bytes <<<"$metadata"
  [ -n "$profile" ] || return 1
  [ -n "$version" ] || return 1
  [[ "$inventory_bytes" =~ ^[0-9]+$ ]] || return 1
  PUBLISHED_PROCESS_FAMILY_VERSION="$version"
  PUBLISHED_PROCESS_FAMILY_VERIFIER_AUTHORITY="$verifier_source"
  generation_id="${manifest_id#sha256:}"

  ensure_installer_process_family_root || return 1
  generation="$generations/$generation_id"
  if [ -e "$generation" ] || [ -L "$generation" ]; then
    [ -d "$generation" ] && [ ! -L "$generation" ] && \
      verify_canonical_generation "$generation" "$version" "$verifier_source" || return 1
  else
    require_filesystem_capacity "$generations" \
      "$((inventory_bytes + INSTALLER_FREE_SPACE_HEADROOM_BYTES))" \
      "immutable process-family generation" || return 1
    # The deterministic stage is a durable retry address. A kill at any
    # prefix is resumed only when every retained member is an exact byte match;
    # malformed residue is preserved and fails closed instead of allocating an
    # unbounded succession of PID-named full-family stages.
    stage_name=".generation-${generation_id}.installing"
    stage="$generations/$stage_name"
    if [ -e "$stage" ] || [ -L "$stage" ]; then
      [ -d "$stage" ] && [ ! -L "$stage" ] || return 1
      if ! bash "$verifier_source" verify --root "$stage" \
          --manifest "$stage/process-family.component-manifest.json" >/dev/null 2>&1; then
        [ "$(installer_stage_mode "$stage")" = 0700 ] || return 1
        validate_installer_stage_inventory "$stage" generation || return 1
      fi
    else
      mkdir -m 0700 "$stage" || return 1
    fi
    ensure_exact_staged_file "$ft_source" "$stage/ft" 0555 || return 1
    ensure_exact_staged_file "$mux_source" "$stage/frankenterm-mux-server" 0555 || return 1
    ensure_exact_staged_file "$guardian_source" "$stage/frankenterm-pty-guardian" 0555 || return 1
    ensure_exact_staged_file "$verifier_source" "$stage/verify-components.sh" 0555 || return 1
    ensure_exact_staged_file "$manifest_source" "$stage/process-family.component-manifest.json" 0444 || return 1
    bash "$verifier_source" verify --root "$stage" \
      --manifest "$stage/process-family.component-manifest.json" >/dev/null || return 1
    "$stage/ft" --version >/dev/null 2>&1 || return 1
    "$stage/frankenterm-mux-server" --version >/dev/null 2>&1 || return 1
    "$stage/frankenterm-pty-guardian" --version >/dev/null 2>&1 || return 1
    chmod 0555 "$stage" || return 1
    fsync_installer_tree "$stage" || return 1
    stage_id=$(atomic_path_content_id "$helper" "$generations" "$stage_name") || return 1
    txid=$(atomic_transition_txid "generation:$DEST:$generation_id") || return 1
    atomic_path_transition "$helper" "$generations" "$stage_name" "$generation_id" \
      "$txid" "$stage_id" missing publish-noreplace || return 1
    verify_canonical_generation "$generation" "$version" "$verifier_source" || return 1
  fi
  installer_failpoint after-generation-publish
  PUBLISHED_PROCESS_FAMILY_ROOT="$generation"

  local current_target="" selected_generation="" initial_install=0 legacy_owner=0
  local authority_state name
  authority_state=$(inspect_installer_process_family_authority) || {
    err "Process-family selector authority is ambiguous or unsafe"
    return 1
  }
  case "$authority_state" in
    initial)
      initial_install=1
      selected_generation="$generation"
      require_no_live_mux_for_initial_selector || true
      [ -n "$INITIAL_SELECTOR_HOLD_REASON" ] || return 1
      ;;
    legacy)
      # Preserve a coherent direct-install family as an immutable recovery
      # generation, but do not replace its entrypoints or manufacture a
      # selector. Only the future cross-launcher activation transaction may
      # move that live authority to the candidate.
      legacy_owner=1
      local legacy_proof="$TMP/legacy-family-proof"
      local legacy_manifest="$TMP/legacy-family.json"
      local legacy_manifest_id legacy_id legacy_stage_name legacy_stage
      mkdir -m 0700 "$legacy_proof" || return 1
      for name in ft frankenterm-mux-server frankenterm-pty-guardian; do
        ensure_exact_staged_file "$DEST/$name" "$legacy_proof/$name" 0555 || return 1
      done
      legacy_manifest_id=$(legacy_process_family_manifest "$legacy_proof" "$legacy_manifest") || return 1
      legacy_id="legacy-${legacy_manifest_id#sha256:}"
      selected_generation="$generations/$legacy_id"
      if [ -e "$selected_generation" ] || [ -L "$selected_generation" ]; then
        [ -d "$selected_generation" ] && [ ! -L "$selected_generation" ] || return 1
        [ "$(legacy_process_family_manifest "$selected_generation" -)" = "$legacy_manifest_id" ] || return 1
      else
        legacy_stage_name=".${legacy_id}.installing"
        legacy_stage="$generations/$legacy_stage_name"
        if [ -e "$legacy_stage" ] || [ -L "$legacy_stage" ]; then
          [ -d "$legacy_stage" ] && [ ! -L "$legacy_stage" ] || return 1
          if [ "$(legacy_process_family_manifest "$legacy_stage" - 2>/dev/null || true)" != \
               "$legacy_manifest_id" ]; then
            [ "$(installer_stage_mode "$legacy_stage")" = 0700 ] || return 1
            validate_installer_stage_inventory "$legacy_stage" legacy || return 1
          fi
        else
          mkdir -m 0700 "$legacy_stage" || return 1
        fi
        for name in ft frankenterm-mux-server frankenterm-pty-guardian; do
          ensure_exact_staged_file "$legacy_proof/$name" "$legacy_stage/$name" 0555 || return 1
        done
        ensure_exact_staged_file "$legacy_manifest" "$legacy_stage/legacy-family.json" 0444 || return 1
        [ "$(legacy_process_family_manifest "$legacy_stage" -)" = "$legacy_manifest_id" ] || return 1
        chmod 0555 "$legacy_stage" || return 1
        fsync_installer_tree "$legacy_stage" || return 1
        stage_id=$(atomic_path_content_id "$helper" "$generations" "$legacy_stage_name") || return 1
        txid=$(atomic_transition_txid "legacy-generation:$DEST:$legacy_id") || return 1
        atomic_path_transition "$helper" "$generations" "$legacy_stage_name" "$legacy_id" \
          "$txid" "$stage_id" missing publish-noreplace || return 1
      fi
      installer_failpoint after-legacy-recovery-publish
      [ "$(inspect_installer_process_family_authority)" = legacy ] || return 1
      for name in ft frankenterm-mux-server frankenterm-pty-guardian; do
        cmp "$DEST/$name" "$selected_generation/$name" >/dev/null 2>&1 || return 1
      done
      ;;
    managed$'\t'*)
      current_target="${authority_state#*$'\t'}"
      selected_generation="$managed/$current_target"
      if [[ "$current_target" =~ ^generations/[0-9a-f]{64}$ ]]; then
        verify_canonical_generation "$selected_generation" "" "$verifier_source" || return 1
      else
        [ -f "$selected_generation/legacy-family.json" ] || return 1
        [ "$(legacy_process_family_manifest "$selected_generation" -)" = \
          "sha256:${current_target##*legacy-}" ] || return 1
      fi
      ;;
    *)
      err "Process-family selector authority returned an invalid state"
      return 1
      ;;
  esac

  if [ "$current_target" = "generations/$generation_id" ]; then
    [ "$(inspect_installer_process_family_authority)" = \
      $'managed\t'"generations/$generation_id" ] || return 1
    verify_canonical_generation "$generation" "$version" "$verifier_source" || return 1
    for name in ft frankenterm-mux-server frankenterm-pty-guardian; do
      stable_entrypoint_is_managed "$name" || return 1
      cmp "$DEST/$name" "$generation/$name" >/dev/null 2>&1 || return 1
    done
    PROCESS_FAMILY_ACTIVATION_STATE="current"
    PROCESS_FAMILY_ACTIVE_AUTHORITY="managed-selector"
    PROCESS_FAMILY_ACTIVE_ROOT="$generation"
    ok "Verified current atomic process-family generation $generation_id"
  else
    [ "$(inspect_installer_process_family_authority)" = "$authority_state" ] || {
      err "Existing process-family authority changed while the candidate was published"
      return 1
    }
    if [ "$initial_install" -eq 1 ]; then
      for name in ft frankenterm-mux-server frankenterm-pty-guardian; do
        if [ -L "$DEST/$name" ]; then
          stable_entrypoint_is_managed "$name" && [ ! -e "$DEST/$name" ] || return 1
        else
          [ ! -e "$DEST/$name" ] || return 1
        fi
      done
      PROCESS_FAMILY_ACTIVE_AUTHORITY="none"
      PROCESS_FAMILY_ACTIVE_ROOT=""
      PROCESS_FAMILY_PENDING_REASON="$INITIAL_SELECTOR_HOLD_REASON"
    elif [ "$legacy_owner" -eq 1 ]; then
      for name in ft frankenterm-mux-server frankenterm-pty-guardian; do
        cmp "$DEST/$name" "$selected_generation/$name" >/dev/null 2>&1 || return 1
      done
      PROCESS_FAMILY_ACTIVE_AUTHORITY="legacy-direct"
      PROCESS_FAMILY_ACTIVE_ROOT="$DEST"
      PROCESS_FAMILY_PENDING_REASON="cross-launcher-lease-required"
    else
      for name in ft frankenterm-mux-server frankenterm-pty-guardian; do
        stable_entrypoint_is_managed "$name" || return 1
        cmp "$DEST/$name" "$selected_generation/$name" >/dev/null 2>&1 || return 1
      done
      PROCESS_FAMILY_ACTIVE_AUTHORITY="managed-selector"
      PROCESS_FAMILY_ACTIVE_ROOT="$selected_generation"
      PROCESS_FAMILY_PENDING_REASON="cross-launcher-lease-required"
    fi
    installer_failpoint before-pending-publication-receipt
    PENDING_PROCESS_FAMILY_GENERATION="$generation_id"
    PROCESS_FAMILY_ACTIVATION_STATE="pending"
    ok "Published immutable process-family candidate $generation_id"
    warn "Activation is pending; the existing process-family selector and live mux were left unchanged"
  fi
  info "All previous generations and entrypoint authority were retained for recovery"
}

# ───────────────────────────────────────────────────────────────────────────
# Explicit activation of a published candidate generation (ft-xxfwy.3).
#
# The automatic install path never publishes the selector: a process census
# is evidence, not exclusion (see require_no_live_mux_for_initial_selector),
# so every fresh candidate is left `pending` and the stable `ft` path stays
# absent. This subcommand is the documented way to finish the job. It moves
# the live authority to a verified candidate only when the operator attests
# that no FrankenTerm GUI, mux server, PTY guardian, or watcher is running
# (--idle-host-confirmed) AND the census agrees it cannot see one. Every
# mutation reuses the descriptor-pinned atomic transitions that candidate
# publication uses, so a crash leaves either the previous authority or the
# new one, never a torn triplet.
#
# Failpoints (FT_INSTALL_TEST_ENABLE_FAILPOINTS=1 FT_INSTALL_TEST_FAILPOINT=...):
#   before-selector-activation, after-selector-activation,
#   before-entrypoint-activation:<name>
# ───────────────────────────────────────────────────────────────────────────
activate_process_family_generation() {
  local generation_id="$1"
  local managed="$DEST/.frankenterm-process-family"
  local generations="$managed/generations"
  local generation="$generations/$generation_id"
  local helper verifier manifest metadata manifest_id build_id source_revision
  local version target profile feature_contract inventory_bytes
  local census authority_state txid stage stage_id target_id operation name
  local legacy_root candidate

  command -v python3 >/dev/null 2>&1 || {
    err "python3 is required for crash-atomic activation"
    return 1
  }
  [[ "$generation_id" =~ ^[0-9a-f]{64}$ ]] || {
    err "--activate expects the 64-hex candidate generation id printed in the install receipt"
    return 2
  }
  [ -d "$generation" ] && [ ! -L "$generation" ] || {
    err "No published candidate generation at $generation"
    err "Run the installer first; it publishes the candidate and prints its id"
    return 1
  }
  if [ "$IDLE_HOST_CONFIRMED" -ne 1 ]; then
    err "Activation replaces the live process-family authority under $DEST."
    err "Quit every FrankenTerm window, frankenterm-mux-server, PTY guardian, and ft watcher,"
    err "then rerun with --idle-host-confirmed to attest that the host is idle."
    return 1
  fi
  census=$(installer_mux_ownership_state) || census=ambiguous
  if [ "$census" != inactive ]; then
    err "Mux ownership census reports '$census'; refusing activation while a FrankenTerm/WezTerm launcher may own session state"
    return 1
  fi

  helper="$generation/ft"
  verifier="$generation/verify-components.sh"
  manifest="$generation/process-family.component-manifest.json"
  verify_canonical_generation "$generation" "" "$verifier" || {
    err "Candidate generation $generation_id failed canonical verification; refusing to activate it"
    return 1
  }
  metadata=$(process_family_manifest_metadata "$manifest" triplet) || return 1
  IFS=$'\t' read -r manifest_id build_id source_revision version target profile feature_contract inventory_bytes <<<"$metadata"

  authority_state=$(inspect_installer_process_family_authority) || {
    err "Process-family selector authority is ambiguous or unsafe"
    return 1
  }
  if [ "$authority_state" = $'managed\t'"generations/$generation_id" ]; then
    info "Generation $generation_id is already the current selector target"
  else
    # 1. Point the selector at the candidate: publish when absent, exchange
    #    when a previous generation is current. Both are single atomic
    #    transitions performed by the candidate's own ft helper.
    txid=$(atomic_transition_txid "selector:$DEST:$generation_id") || return 1
    stage=".current.${txid}"
    ensure_staged_symlink "generations/$generation_id" "$managed/$stage" || return 1
    stage_id=$(atomic_path_content_id "$helper" "$managed" "$stage") || return 1
    if [ -L "$managed/current" ] || [ -e "$managed/current" ]; then
      target_id=$(atomic_path_content_id "$helper" "$managed" current) || return 1
      operation=exchange
    else
      target_id=missing
      operation=publish-noreplace
    fi
    installer_failpoint before-selector-activation
    atomic_path_transition "$helper" "$managed" "$stage" current "$txid" \
      "$stage_id" "$target_id" "$operation" || {
      err "Selector activation failed; the previous authority is unchanged"
      return 1
    }
    installer_failpoint after-selector-activation
  fi

  # 2. Publish the three stable entrypoints as managed symlinks. A legacy
  #    direct-install binary is exchanged only when a retained legacy
  #    generation proves it is the exact file the installer captured.
  for name in ft frankenterm-mux-server frankenterm-pty-guardian; do
    installer_failpoint "before-entrypoint-activation:$name"
    if stable_entrypoint_is_managed "$name"; then
      publish_stable_entrypoint "$helper" "$name" missing "$generation" || {
        err "Managed entrypoint $name does not resolve to generation $generation_id"
        return 1
      }
    elif [ -e "$DEST/$name" ] || [ -L "$DEST/$name" ]; then
      [ -f "$DEST/$name" ] && [ ! -L "$DEST/$name" ] || {
        err "Refusing to replace unmanaged entrypoint $DEST/$name (not a regular file)"
        return 1
      }
      legacy_root=""
      for candidate in "$generations"/legacy-*/; do
        [ -d "$candidate" ] || continue
        if cmp "$DEST/$name" "${candidate%/}/$name" >/dev/null 2>&1; then
          legacy_root="${candidate%/}"
          break
        fi
      done
      [ -n "$legacy_root" ] || {
        err "Direct-install entrypoint $DEST/$name matches no retained legacy generation; rerun the installer to capture it before activating"
        return 1
      }
      publish_stable_entrypoint "$helper" "$name" exchange "$legacy_root" || {
        err "Failed to exchange legacy entrypoint $name for the managed selector"
        return 1
      }
    else
      publish_stable_entrypoint "$helper" "$name" missing "$generation" || {
        err "Failed to publish managed entrypoint $name"
        return 1
      }
    fi
  done

  # 3. Prove the result before claiming it.
  [ "$(inspect_installer_process_family_authority)" = $'managed\t'"generations/$generation_id" ] || {
    err "Selector authority did not settle on generation $generation_id"
    return 1
  }
  for name in ft frankenterm-mux-server frankenterm-pty-guardian; do
    stable_entrypoint_is_managed "$name" || {
      err "Stable entrypoint $name is not a managed selector link"
      return 1
    }
    cmp "$DEST/$name" "$generation/$name" >/dev/null 2>&1 || {
      err "Stable entrypoint $name does not resolve to generation $generation_id"
      return 1
    }
  done
  "$DEST/ft" --version >/dev/null 2>&1 || {
    err "Activated ft does not execute from $DEST"
    return 1
  }

  PROCESS_FAMILY_ACTIVATION_STATE="current"
  PROCESS_FAMILY_ACTIVE_AUTHORITY="managed-selector"
  PROCESS_FAMILY_ACTIVE_ROOT="$generation"
  PROCESS_FAMILY_PENDING_REASON=""
  PENDING_PROCESS_FAMILY_GENERATION=""
  PUBLISHED_PROCESS_FAMILY_ROOT="$generation"
  PUBLISHED_PROCESS_FAMILY_VERSION="$version"
  ok "Activated process-family generation $generation_id (v$version) as the current authority under $DEST"
  return 0
}

emit_process_family_receipt() {
  python3 - "$PROCESS_FAMILY_ACTIVATION_STATE" "$PROCESS_FAMILY_ACTIVE_AUTHORITY" \
    "$PROCESS_FAMILY_ACTIVE_ROOT" "$PROCESS_FAMILY_PENDING_REASON" \
    "$PENDING_PROCESS_FAMILY_GENERATION" "$PUBLISHED_PROCESS_FAMILY_ROOT" \
    "$PUBLISHED_PROCESS_FAMILY_VERSION" <<'PY'
import json, os, re, sys

(
    activation, active_authority, active_root, pending_reason,
    generation, candidate_root, version,
) = sys.argv[1:]
if activation not in ("current", "pending"):
    raise SystemExit("process-family activation receipt has an invalid state")
if active_authority not in ("none", "legacy-direct", "managed-selector"):
    raise SystemExit("process-family activation receipt has an invalid authority")
if activation == "pending" and re.fullmatch(r"[0-9a-f]{64}", generation) is None:
    raise SystemExit("pending process-family receipt has an invalid generation")
if activation == "current" and generation:
    raise SystemExit("current process-family receipt unexpectedly names a pending generation")
if activation == "pending" and not pending_reason:
    raise SystemExit("pending process-family receipt lacks its precise hold reason")
if activation == "current" and pending_reason:
    raise SystemExit("current process-family receipt unexpectedly names a pending reason")
if any(ord(character) < 0x20 for character in pending_reason):
    raise SystemExit("process-family activation receipt has an invalid pending reason")
if active_authority == "none":
    if active_root:
        raise SystemExit("absent process-family authority unexpectedly names an active root")
    active_root_value = None
else:
    if not active_root:
        raise SystemExit("process-family authority lacks its active root")
    active_root_value = os.path.abspath(active_root)
if not version or any(ord(character) < 0x20 for character in version):
    raise SystemExit("process-family activation receipt has an invalid version")
candidate_root = os.path.abspath(candidate_root)
payload = {
    "activation": activation,
    "active_authority": active_authority,
    "active_root": active_root_value,
    "candidate_generation": generation or os.path.basename(candidate_root),
    "candidate_root": candidate_root,
    "candidate_version": version,
    "pending_reason": pending_reason or None,
    "schema_version": "frankenterm.install.process-family-receipt.v1",
}
print("FT_INSTALL_PROCESS_FAMILY_RECEIPT_V1=" + json.dumps(
    payload, sort_keys=True, separators=(",", ":"), ensure_ascii=True,
))
PY
}

emit_app_receipt() {
  python3 - "$APP_RECEIPT_REQUESTED" "$APP_RECEIPT_RESULT" \
    "$APP_RECEIPT_REASON" "$APP_RECEIPT_MANIFEST_ID" \
    "$APP_RECEIPT_CANDIDATE_PATH" "$APP_RECEIPT_READINESS" \
    "${APP_ACTIVATION_STATE:-none}" <<'PY'
import json, os, re, sys

requested, result, reason, manifest_id, candidate_path, readiness, activation = sys.argv[1:]
if requested not in ("true", "false"):
    raise SystemExit("app receipt has an invalid requested flag")
if result not in ("not_requested", "skipped", "verified"):
    raise SystemExit("app receipt has an invalid result")
if re.fullmatch(r"[a-z0-9_]+", reason) is None:
    raise SystemExit("app receipt has an invalid reason")
if readiness not in ("not_run", "running", "failed", "passed", "existing_manifest_verified"):
    raise SystemExit("app receipt has an invalid readiness state")
if activation not in ("none", "pending", "current"):
    raise SystemExit("app receipt has an invalid activation state")
if manifest_id and re.fullmatch(r"sha256:[0-9a-f]{64}", manifest_id) is None:
    raise SystemExit("app receipt has an invalid manifest identity")
if candidate_path:
    candidate_path = os.path.abspath(candidate_path)
if result == "verified":
    if not manifest_id or not candidate_path or activation not in ("pending", "current"):
        raise SystemExit("verified app receipt lacks candidate authority")
elif activation != "none":
    raise SystemExit("non-verified app receipt claims active authority")
if result == "not_requested" and requested != "false":
    raise SystemExit("not-requested app receipt claims a request")
payload = {
    "activation": activation,
    "candidate_path": candidate_path or None,
    "manifest_id": manifest_id or None,
    "readiness": readiness,
    "reason": reason,
    "requested": requested == "true",
    "result": result,
    "schema_version": "frankenterm.install.app-receipt.v1",
}
print("FT_INSTALL_APP_RECEIPT_V1=" + json.dumps(
    payload, sort_keys=True, separators=(",", ":"), ensure_ascii=True,
))
PY
}

mark_app_not_selected() {
  local reason="$1"
  APP_RECEIPT_REQUESTED="false"
  APP_RECEIPT_RESULT="not_requested"
  APP_RECEIPT_REASON="$reason"
  APP_RECEIPT_MANIFEST_ID=""
  APP_RECEIPT_CANDIDATE_PATH=""
  APP_RECEIPT_READINESS="not_run"
  APP_ACTIVATION_STATE="none"
}

mark_app_skipped() {
  local reason="$1"
  APP_RECEIPT_RESULT="skipped"
  APP_RECEIPT_REASON="$reason"
  APP_ACTIVATION_STATE="none"
}

finalize_app_receipt_state() {
  if [ "$APP_RECEIPT_RESULT" = in_progress ]; then
    mark_app_skipped app_install_incomplete
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
  if [ -e "$DEST/ft" ] || [ -L "$DEST/ft" ]; then
    info "Existing ft path detected; it will not be executed before authenticated family verification"
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
  # A retained generation cannot authenticate its own verifier: a same-UID
  # mutation could replace both executable bytes and that verifier. Refresh
  # from the outer-checksum-authenticated release package instead of claiming
  # a tamper-proof same-version fast path without an external trust root.
  return 1
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
# Checksum + DSR minisign verification
# ───────────────────────────────────────────────────────────────────────────
verify_checksum() {
  local file="$1" expected="$2" proof="" actual="" identity=""
  VERIFIED_ARCHIVE_IDENTITY=""
  [[ "$expected" =~ ^[0-9a-f]{64}$ ]] || {
    err "Expected checksum is not one canonical lowercase SHA-256 digest"
    return 1
  }
  proof=$(python3 - "$file" <<'PY'
import hashlib, os, stat, sys

path = sys.argv[1]
flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0)
fd = os.open(path, flags)
try:
    before = os.fstat(fd)
    named_before = os.stat(path, follow_symlinks=False)
    if (not stat.S_ISREG(before.st_mode) or before.st_nlink != 1 or
            before.st_size > 4 * 1024 * 1024 * 1024 or
            (before.st_dev, before.st_ino) != (named_before.st_dev, named_before.st_ino)):
        raise SystemExit("checksum target is not one bounded single-link regular file")
    digest = hashlib.sha256()
    remaining = before.st_size
    while remaining:
        chunk = os.read(fd, min(1024 * 1024, remaining))
        if not chunk:
            raise SystemExit("checksum target truncated while hashed")
        digest.update(chunk)
        remaining -= len(chunk)
    if os.read(fd, 1):
        raise SystemExit("checksum target grew while hashed")
    after = os.fstat(fd)
    named_after = os.stat(path, follow_symlinks=False)
    identity = (
        before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns,
    )
    if identity != (
            after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns,
    ) or (after.st_dev, after.st_ino) != (named_after.st_dev, named_after.st_ino):
        raise SystemExit("checksum target changed while hashed through its authority descriptor")
    print(f"{digest.hexdigest()}\t" + ":".join(str(value) for value in identity))
finally:
    os.close(fd)
PY
  ) || { err "Unsafe or unstable checksum target: $file"; return 1; }
  IFS=$'\t' read -r actual identity <<<"$proof"
  [[ "$actual" =~ ^[0-9a-f]{64}$ ]] && [ -n "$identity" ] || return 1
  if [ "$actual" != "$expected" ]; then
    err "Checksum verification FAILED"
    err "Expected: $expected"
    err "Got:      $actual"
    err "The downloaded file may be corrupted or tampered with."
    return 1
  fi
  VERIFIED_ARCHIVE_IDENTITY="$identity"
  ok "Checksum verified: ${actual:0:16}..."
  return 0
}

read_sha256_sidecar() {
  local sidecar="$1" expected_name="$2"
  python3 - "$sidecar" "$expected_name" <<'PY'
import os, re, stat, sys

path, expected_name = sys.argv[1:]
fd = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0))
try:
    before = os.fstat(fd)
    named = os.stat(path, follow_symlinks=False)
    if (not stat.S_ISREG(before.st_mode) or before.st_nlink != 1 or
            before.st_size > 4096 or
            (before.st_dev, before.st_ino) != (named.st_dev, named.st_ino)):
        raise SystemExit("checksum sidecar is not one bounded single-link regular file")
    chunks = []
    remaining = before.st_size
    while remaining:
        chunk = os.read(fd, min(4097, remaining))
        if not chunk:
            raise SystemExit("checksum sidecar truncated while read")
        chunks.append(chunk)
        remaining -= len(chunk)
    if os.read(fd, 1):
        raise SystemExit("checksum sidecar grew while read")
    payload = b"".join(chunks)
    after = os.fstat(fd)
    if (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns) != (
            after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns):
        raise SystemExit("checksum sidecar changed while read")
finally:
    os.close(fd)
try:
    text = payload.decode("ascii")
except UnicodeDecodeError as error:
    raise SystemExit("checksum sidecar is not ASCII") from error
lines = text.splitlines()
if len(lines) != 1:
    raise SystemExit("checksum sidecar must contain exactly one record")
match = re.fullmatch(r"([0-9a-f]{64})(?:[ \t]+\*?([^\r\n]+))?", lines[0])
if match is None:
    raise SystemExit("checksum sidecar record is not canonical SHA-256")
recorded_name = match.group(2)
if recorded_name is not None and recorded_name != expected_name:
    raise SystemExit("checksum sidecar names a different archive")
print(match.group(1))
PY
}

verify_archive_checksum_authority() {
  local archive="$1" archive_name="$2" sidecar=""
  if [ -n "$CHECKSUM" ]; then
    [[ "$CHECKSUM" =~ ^[0-9a-f]{64}$ ]] || {
      err "--checksum must be one canonical lowercase SHA-256 digest"
      return 1
    }
  elif [ -n "$OFFLINE_TARBALL" ]; then
    sidecar="${OFFLINE_TARBALL}.sha256"
    [ -f "$sidecar" ] && [ ! -L "$sidecar" ] || {
      err "Offline archives require --checksum HEX or an adjacent authenticated sidecar"
      err "Expected sidecar: $sidecar"
      return 1
    }
    CHECKSUM=$(read_sha256_sidecar "$sidecar" "$(basename "$OFFLINE_TARBALL")") || {
      err "Offline archive checksum sidecar is invalid or names another artifact"
      return 1
    }
    info "Using externally supplied offline checksum sidecar: $sidecar"
  else
    [ -z "$CHECKSUM_URL" ] && CHECKSUM_URL="${URL}.sha256"
    case "$CHECKSUM_URL" in
      https://*) ;;
      *)
        err "Remote checksum authority must use HTTPS; pass --checksum for an out-of-band digest"
        return 1
        ;;
    esac
    info "Fetching checksum from $CHECKSUM_URL"
    if ! download_https_bounded "$CHECKSUM_URL" "$TMP/checksum.sha256" 4096 30; then
      err "Checksum required and could not be fetched"
      return 1
    fi
    CHECKSUM=$(read_sha256_sidecar "$TMP/checksum.sha256" "$archive_name") || {
      err "Downloaded checksum sidecar is invalid or names another artifact"
      return 1
    }
  fi
  verify_checksum "$archive" "$CHECKSUM"
}

process_family_input_receipt() {
  local manifest="$1" role="$2" expected_path="$3" max_bytes="$4" expected_id="$5"
  python3 - "$manifest" "$role" "$expected_path" "$max_bytes" "$expected_id" <<'PY'
import hashlib, json, os, re, stat, sys

manifest_path, expected_role, expected_path, maximum_bytes_text, expected_id = sys.argv[1:]
try:
    maximum_bytes = int(maximum_bytes_text)
except ValueError as error:
    raise SystemExit("input receipt byte bound is malformed") from error
if maximum_bytes <= 0 or str(maximum_bytes) != maximum_bytes_text:
    raise SystemExit("input receipt byte bound is non-canonical")
if re.fullmatch(r"[0-9a-f]{64}", expected_id) is None:
    raise SystemExit("expected process-family manifest identity is non-canonical")

nofollow = getattr(os, "O_NOFOLLOW", 0)
cloexec = getattr(os, "O_CLOEXEC", 0)
if not nofollow:
    raise SystemExit("descriptor-relative nofollow manifest reads are unavailable")
fd = os.open(manifest_path, os.O_RDONLY | nofollow | cloexec)
try:
    before = os.fstat(fd)
    named_before = os.stat(manifest_path, follow_symlinks=False)
    if (not stat.S_ISREG(before.st_mode) or before.st_nlink != 1 or
            before.st_size > 64 * 1024 * 1024 or
            (before.st_dev, before.st_ino) != (named_before.st_dev, named_before.st_ino)):
        raise SystemExit("process-family manifest is not one bounded single-link regular file")
    chunks = []
    remaining = before.st_size
    while remaining:
        chunk = os.read(fd, min(1024 * 1024, remaining))
        if not chunk:
            raise SystemExit("process-family manifest truncated while read")
        chunks.append(chunk)
        remaining -= len(chunk)
    if os.read(fd, 1):
        raise SystemExit("process-family manifest grew while read")
    after = os.fstat(fd)
    named_after = os.stat(manifest_path, follow_symlinks=False)
    if ((before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns,
         before.st_ctime_ns) !=
        (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns,
         after.st_ctime_ns) or
        (after.st_dev, after.st_ino) != (named_after.st_dev, named_after.st_ino)):
        raise SystemExit("process-family manifest changed while its input receipt was read")
finally:
    os.close(fd)

def unique_object(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise SystemExit("process-family manifest contains a duplicate JSON key")
        value[key] = item
    return value

try:
    manifest = json.loads(b"".join(chunks).decode("utf-8"), object_pairs_hook=unique_object)
except (UnicodeDecodeError, json.JSONDecodeError) as error:
    raise SystemExit("process-family manifest is not canonical UTF-8 JSON") from error
if not isinstance(manifest, dict) or manifest.get("schema_version") != "ft.atomic_component_manifest.v1":
    raise SystemExit("process-family manifest has an unexpected schema")
claimed_id = manifest.get("manifest_id")
content = dict(manifest)
content.pop("manifest_id", None)
actual_id = "sha256:" + hashlib.sha256(json.dumps(
    content,
    ensure_ascii=False,
    separators=(",", ":"),
    sort_keys=True,
).encode("utf-8")).hexdigest()
if claimed_id != f"sha256:{expected_id}" or actual_id != claimed_id:
    raise SystemExit("process-family manifest no longer matches its immutable generation identity")
inputs = manifest.get("inputs")
if not isinstance(inputs, list) or len(inputs) > 10_000:
    raise SystemExit("process-family manifest input catalog is not bounded")
matches = []
for record in inputs:
    if not isinstance(record, dict) or set(record) != {"bytes", "path", "role", "sha256"}:
        raise SystemExit("process-family manifest has a malformed input receipt")
    if record.get("role") == expected_role:
        matches.append(record)
if len(matches) != 1:
    raise SystemExit("process-family manifest lacks one exact font payload receipt")
record = matches[0]
if record["path"] != expected_path:
    raise SystemExit("process-family manifest font receipt names an unexpected source path")
byte_count = record["bytes"]
digest = record["sha256"]
if (type(byte_count) is not int or byte_count <= 0 or byte_count > maximum_bytes or
        not isinstance(digest, str) or re.fullmatch(r"[0-9a-f]{64}", digest) is None):
    raise SystemExit("process-family manifest font receipt exceeds its canonical bounds")
print(f"{digest}\t{byte_count}")
PY
}

prepare_font_generation_stage() {
  local parent="$1" stage_name="$2"
  python3 - "$parent" "$stage_name" <<'PY'
import os, re, stat, sys

parent_path, stage_name = sys.argv[1:]
if re.fullmatch(r"\.pragmasevka-[0-9a-f]{64}\.installing(?:-[0-9]{1,3})?", stage_name) is None:
    raise SystemExit("font generation stage name is non-canonical")
expected = {
    "pragmasevka-nf-regular.ttf",
    "pragmasevka-nf-bold.ttf",
    "pragmasevka-nf-italic.ttf",
    "pragmasevka-nf-bolditalic.ttf",
}
nofollow = getattr(os, "O_NOFOLLOW", 0)
directory = getattr(os, "O_DIRECTORY", 0)
cloexec = getattr(os, "O_CLOEXEC", 0)
if not nofollow or not directory:
    raise SystemExit("descriptor-relative nofollow font staging is unavailable")
parent_named = os.stat(parent_path, follow_symlinks=False)
parent_fd = os.open(parent_path, os.O_RDONLY | directory | nofollow | cloexec)
stage_fd = -1
try:
    parent_opened = os.fstat(parent_fd)
    if (not stat.S_ISDIR(parent_opened.st_mode) or
            parent_opened.st_uid != os.geteuid() or
            stat.S_IMODE(parent_opened.st_mode) & 0o022 or
            (parent_opened.st_dev, parent_opened.st_ino) !=
            (parent_named.st_dev, parent_named.st_ino)):
        raise SystemExit("font stage parent is not one private owner-controlled directory")
    try:
        os.mkdir(stage_name, 0o700, dir_fd=parent_fd)
        os.fsync(parent_fd)
    except FileExistsError:
        pass
    stage_named = os.stat(stage_name, dir_fd=parent_fd, follow_symlinks=False)
    stage_fd = os.open(
        stage_name, os.O_RDONLY | directory | nofollow | cloexec, dir_fd=parent_fd)
    stage_opened = os.fstat(stage_fd)
    if (not stat.S_ISDIR(stage_opened.st_mode) or
            stage_opened.st_uid != os.geteuid() or
            stat.S_IMODE(stage_opened.st_mode) not in (0o700, 0o555) or
            (stage_opened.st_dev, stage_opened.st_ino) !=
            (stage_named.st_dev, stage_named.st_ino)):
        raise SystemExit("retained font stage is not one resumable private directory")
    os.fchmod(stage_fd, 0o700)
    names = []
    with os.scandir(stage_fd) as entries:
        for entry in entries:
            names.append(entry.name)
    if len(names) > len(expected) or any(name not in expected for name in names):
        raise SystemExit("retained font stage has a noncanonical inventory")
    for name in names:
        named = os.stat(name, dir_fd=stage_fd, follow_symlinks=False)
        fd = os.open(name, os.O_RDONLY | nofollow | cloexec, dir_fd=stage_fd)
        try:
            opened = os.fstat(fd)
            if (not stat.S_ISREG(opened.st_mode) or opened.st_uid != os.geteuid() or
                    opened.st_nlink != 1 or opened.st_size > 1024 * 1024 * 1024 or
                    stat.S_IMODE(opened.st_mode) not in (0o600, 0o444) or
                    (opened.st_dev, opened.st_ino) != (named.st_dev, named.st_ino)):
                raise SystemExit("retained font stage contains an unsafe file")
        finally:
            os.close(fd)
    os.fsync(stage_fd)
    os.fsync(parent_fd)
    parent_final = os.stat(parent_path, follow_symlinks=False)
    if (parent_opened.st_dev, parent_opened.st_ino) != (
            parent_final.st_dev, parent_final.st_ino):
        raise SystemExit("font stage parent detached while prepared")
finally:
    if stage_fd >= 0:
        os.close(stage_fd)
    os.close(parent_fd)
PY
}

verify_font_tree_receipt() {
  local root="$1" receipt="$2" require_sealed="${3:-0}"
  python3 - "$root" "$receipt" "$require_sealed" <<'PY'
import base64, binascii, hashlib, json, os, re, stat, sys

root_path, receipt, require_sealed_text = sys.argv[1:]
if require_sealed_text not in ("0", "1"):
    raise SystemExit("font receipt seal requirement is invalid")
require_sealed = require_sealed_text == "1"
prefix = "FT_FONT_TREE_RECEIPT_V1="
if not receipt.startswith(prefix) or "\n" in receipt or "\r" in receipt:
    raise SystemExit("font tree receipt envelope is malformed")
encoded = receipt[len(prefix):]
try:
    payload = base64.b64decode(encoded, altchars=b"-_", validate=True)
except (ValueError, binascii.Error) as error:
    raise SystemExit("font tree receipt is not canonical base64url") from error
if base64.urlsafe_b64encode(payload).decode("ascii") != encoded:
    raise SystemExit("font tree receipt base64url is non-canonical")
try:
    expected_records = json.loads(payload.decode("utf-8"))
except (UnicodeDecodeError, json.JSONDecodeError) as error:
    raise SystemExit("font tree receipt payload is not canonical JSON") from error
expected_names = {
    "pragmasevka-nf-regular.ttf",
    "pragmasevka-nf-bold.ttf",
    "pragmasevka-nf-italic.ttf",
    "pragmasevka-nf-bolditalic.ttf",
}
if (not isinstance(expected_records, list) or len(expected_records) != 4 or
        json.dumps(expected_records, separators=(",", ":"), sort_keys=True).encode("utf-8") !=
        payload):
    raise SystemExit("font tree receipt payload is non-canonical")
seen = set()
for record in expected_records:
    if not isinstance(record, dict) or set(record) != {"bytes", "path", "sha256"}:
        raise SystemExit("font tree receipt record is malformed")
    name = record["path"]
    byte_count = record["bytes"]
    digest = record["sha256"]
    if (name not in expected_names or name in seen or type(byte_count) is not int or
            byte_count <= 0 or byte_count > 1024 * 1024 * 1024 or
            not isinstance(digest, str) or re.fullmatch(r"[0-9a-f]{64}", digest) is None):
        raise SystemExit("font tree receipt record exceeds its exact contract")
    seen.add(name)
if seen != expected_names:
    raise SystemExit("font tree receipt does not name the exact font inventory")

nofollow = getattr(os, "O_NOFOLLOW", 0)
directory = getattr(os, "O_DIRECTORY", 0)
cloexec = getattr(os, "O_CLOEXEC", 0)
root_named = os.stat(root_path, follow_symlinks=False)
root_fd = os.open(root_path, os.O_RDONLY | directory | nofollow | cloexec)
try:
    root_before = os.fstat(root_fd)
    root_mode = stat.S_IMODE(root_before.st_mode)
    if (not stat.S_ISDIR(root_before.st_mode) or root_before.st_uid != os.geteuid() or
            root_mode not in ((0o555,) if require_sealed else (0o700, 0o555)) or
            (root_before.st_dev, root_before.st_ino) != (root_named.st_dev, root_named.st_ino)):
        raise SystemExit("font tree root is not one stable owner-controlled directory")
    with os.scandir(root_fd) as entries:
        names = sorted(entry.name for entry in entries)
    if set(names) != expected_names or len(names) != 4:
        raise SystemExit("font tree does not contain its exact four-file inventory")
    actual_records = []
    for name in names:
        named_before = os.stat(name, dir_fd=root_fd, follow_symlinks=False)
        fd = os.open(name, os.O_RDONLY | nofollow | cloexec, dir_fd=root_fd)
        try:
            before = os.fstat(fd)
            if (not stat.S_ISREG(before.st_mode) or before.st_uid != os.geteuid() or
                    before.st_nlink != 1 or stat.S_IMODE(before.st_mode) != 0o444 or
                    before.st_size <= 0 or before.st_size > 1024 * 1024 * 1024 or
                    (before.st_dev, before.st_ino) != (named_before.st_dev, named_before.st_ino)):
                raise SystemExit("font tree contains an unsafe or unsealed file")
            digest = hashlib.sha256()
            remaining = before.st_size
            while remaining:
                chunk = os.read(fd, min(1024 * 1024, remaining))
                if not chunk:
                    raise SystemExit("font tree file truncated while read")
                digest.update(chunk)
                remaining -= len(chunk)
            if os.read(fd, 1):
                raise SystemExit("font tree file grew while read")
            after = os.fstat(fd)
            named_after = os.stat(name, dir_fd=root_fd, follow_symlinks=False)
            if ((before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns,
                 before.st_ctime_ns) !=
                (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns,
                 after.st_ctime_ns) or
                (after.st_dev, after.st_ino) != (named_after.st_dev, named_after.st_ino)):
                raise SystemExit("font tree file changed while its receipt was verified")
            actual_records.append({
                "bytes": before.st_size,
                "path": name,
                "sha256": digest.hexdigest(),
            })
        finally:
            os.close(fd)
    if actual_records != expected_records:
        raise SystemExit("font tree differs from its authenticated exact receipt")
    root_after = os.fstat(root_fd)
    root_named_after = os.stat(root_path, follow_symlinks=False)
    if ((root_before.st_dev, root_before.st_ino, root_before.st_mtime_ns,
         root_before.st_ctime_ns) !=
        (root_after.st_dev, root_after.st_ino, root_after.st_mtime_ns,
         root_after.st_ctime_ns) or
        (root_after.st_dev, root_after.st_ino) !=
        (root_named_after.st_dev, root_named_after.st_ino)):
        raise SystemExit("font tree root changed while its receipt was verified")
finally:
    os.close(root_fd)
PY
}

seal_font_generation_stage() {
  local root="$1" receipt="$2"
  verify_font_tree_receipt "$root" "$receipt" 0 || return 1
  python3 - "$root" <<'PY'
import os, stat, sys
path = sys.argv[1]
nofollow = getattr(os, "O_NOFOLLOW", 0)
directory = getattr(os, "O_DIRECTORY", 0)
named = os.stat(path, follow_symlinks=False)
fd = os.open(path, os.O_RDONLY | directory | nofollow | getattr(os, "O_CLOEXEC", 0))
try:
    opened = os.fstat(fd)
    if (not stat.S_ISDIR(opened.st_mode) or opened.st_uid != os.geteuid() or
            stat.S_IMODE(opened.st_mode) not in (0o700, 0o555) or
            (opened.st_dev, opened.st_ino) != (named.st_dev, named.st_ino)):
        raise SystemExit("font generation stage changed before sealing")
    os.fchmod(fd, 0o555)
    os.fsync(fd)
finally:
    os.close(fd)
parent = os.path.dirname(os.path.abspath(path))
parent_fd = os.open(parent, os.O_RDONLY | directory | nofollow)
try:
    os.fsync(parent_fd)
finally:
    os.close(parent_fd)
PY
  verify_font_tree_receipt "$root" "$receipt" 1
}

select_font_generation_stage() {
  local parent="$1" archive_digest="$2" receipt="$3"
  local index name path mode
  for ((index=0; index<256; index++)); do
    name=".pragmasevka-${archive_digest}.installing"
    [ "$index" -eq 0 ] || name="${name}-${index}"
    path="$parent/$name"
    if [ ! -e "$path" ] && [ ! -L "$path" ]; then
      printf '%s\n' "$name"
      return 0
    fi
    if [ -d "$path" ] && [ ! -L "$path" ]; then
      if verify_font_tree_receipt "$path" "$receipt" 1 >/dev/null 2>&1; then
        printf '%s\n' "$name"
        return 0
      fi
      mode=$(installer_stage_mode "$path" 2>/dev/null || true)
      if [ "$mode" = 0700 ]; then
        # A deterministic private partial is the only residue eligible for
        # prefix-resume. Conflicting bytes fail closed in the extractor.
        printf '%s\n' "$name"
        return 0
      fi
    fi
  done
  err "Retained font rollback generations exhausted the bounded 256-stage catalog"
  return 1
}

installer_test_mutate_font_stage() {
  local root="$1"
  if [ "${FT_INSTALL_TEST_LIBRARY_ONLY:-0}" = 1 ] &&
      [ "${FT_INSTALL_TEST_ENABLE_FAILPOINTS:-0}" = 1 ] &&
      [ "${FT_INSTALL_TEST_MUTATE_FONT_STAGE:-0}" = 1 ]; then
    python3 - "$root" <<'PY'
import os, stat, sys
root = sys.argv[1]
nofollow = getattr(os, "O_NOFOLLOW", 0)
directory = getattr(os, "O_DIRECTORY", 0)
root_fd = os.open(root, os.O_RDONLY | directory | nofollow)
fd = -1
try:
    name = "pragmasevka-nf-regular.ttf"
    fd = os.open(name, os.O_RDONLY | nofollow, dir_fd=root_fd)
    observed = os.fstat(fd)
    if not stat.S_ISREG(observed.st_mode) or observed.st_uid != os.geteuid():
        raise SystemExit("test mutation target is unsafe")
    os.fchmod(fd, 0o600)
    os.close(fd)
    fd = os.open(name, os.O_RDWR | nofollow, dir_fd=root_fd)
    os.lseek(fd, 0, os.SEEK_END)
    os.write(fd, b"test-only-mutation")
    os.fsync(fd)
finally:
    if fd >= 0:
        os.close(fd)
    os.close(root_fd)
PY
  fi
}

extract_authenticated_archive() {
  local archive="$1" root="$2" kind="$3" manifest_name="$4" expected_identity="${5:-}"
  local archive_action="${6:-extract}"
  local max_archive_bytes max_expanded_bytes
  case "$kind" in
    process-family)
      max_archive_bytes="$MAX_PROCESS_ARCHIVE_BYTES"
      max_expanded_bytes="$MAX_PROCESS_EXPANDED_BYTES"
      ;;
    app)
      max_archive_bytes="$MAX_APP_ARCHIVE_BYTES"
      max_expanded_bytes="$MAX_APP_EXPANDED_BYTES"
      ;;
    font)
      max_archive_bytes="$MAX_FONT_ARCHIVE_BYTES"
      max_expanded_bytes="$MAX_FONT_EXPANDED_BYTES"
      ;;
    *) return 1 ;;
  esac
  [ -n "$expected_identity" ] || {
    err "Authenticated archive identity receipt is absent"
    return 1
  }
  case "$archive_action" in
    extract) ;;
    scan)
      [ "$kind" = font ] || {
        err "Archive scan-only mode is reserved for authenticated font receipts"
        return 1
      }
      ;;
    *) return 1 ;;
  esac
  python3 - "$archive" "$root" "$kind" "$manifest_name" "$expected_identity" \
    "$max_archive_bytes" "$max_expanded_bytes" \
    "$INSTALLER_FREE_SPACE_HEADROOM_BYTES" "$archive_action" <<'PY'
import base64, hashlib, json, lzma, os, posixpath, resource, stat, subprocess, sys, tarfile

archive_path, root_path, archive_kind, manifest_name, expected_identity = sys.argv[1:6]
max_archive_bytes, max_expanded_bytes, free_space_headroom = map(int, sys.argv[6:9])
archive_action = sys.argv[9]
if archive_kind not in ("process-family", "app", "font"):
    raise SystemExit("unknown authenticated archive kind")
if archive_action not in ("extract", "scan") or (
        archive_action == "scan" and archive_kind != "font"):
    raise SystemExit("invalid authenticated archive action")

if archive_kind == "font":
    MAX_ENTRIES = 4096
    MAX_NAME_BYTES = 1024 * 1024
else:
    MAX_ENTRIES = 1_000_000
    MAX_NAME_BYTES = 64 * 1024 * 1024
MAX_LINK_BYTES = 4096
MAX_EXTENSION_MEMBER_BYTES = 1024 * 1024
MAX_EXTENSION_TOTAL_BYTES = 16 * 1024 * 1024
MAX_XZ_DECODER_MEMORY_BYTES = 64 * 1024 * 1024
MAX_PARSER_ADDRESS_SPACE_BYTES = 1024 * 1024 * 1024
MIN_PARSER_ADDRESS_SPACE_BYTES = 128 * 1024 * 1024
MAX_PARSER_CPU_SECONDS = 1800
MAX_IO_CHUNK_BYTES = 1024 * 1024
MAX_RETAINED_METADATA_BYTES = 256 * 1024 * 1024
MAX_ZSTD_BLOCKS = 1_000_000
if (os.environ.get("FT_INSTALL_TEST_LIBRARY_ONLY") == "1" and
        os.environ.get("FT_INSTALL_TEST_ENABLE_RESOURCE_OVERRIDES") == "1"):
    override = os.environ.get("FT_INSTALL_TEST_MAX_RETAINED_METADATA_BYTES")
    if override is not None:
        try:
            MAX_RETAINED_METADATA_BYTES = int(override)
        except ValueError as error:
            raise SystemExit("test metadata bound is malformed") from error
        if MAX_RETAINED_METADATA_BYTES <= 0:
            raise SystemExit("test metadata bound must be positive")
MAX_TAR_STREAM_BYTES = (
    max_expanded_bytes + MAX_ENTRIES * 1024 + MAX_EXTENSION_TOTAL_BYTES + 1024
)
EXPECTED_FONT_FILES = {
    "pragmasevka-nf-regular.ttf",
    "pragmasevka-nf-bold.ttf",
    "pragmasevka-nf-italic.ttf",
    "pragmasevka-nf-bolditalic.ttf",
}
nofollow = getattr(os, "O_NOFOLLOW", 0)
directory = getattr(os, "O_DIRECTORY", 0)
cloexec = getattr(os, "O_CLOEXEC", 0)
if not nofollow or not directory:
    raise SystemExit("descriptor-relative nofollow extraction is unavailable")

def enforce_finite_soft_limit(resource_id, maximum, minimum, label):
    try:
        soft, hard = resource.getrlimit(resource_id)
        candidates = [maximum]
        if soft != resource.RLIM_INFINITY:
            candidates.append(soft)
        if hard != resource.RLIM_INFINITY:
            candidates.append(hard)
        target = min(candidates)
        if target < minimum:
            raise SystemExit(f"{label} limit is too small for bounded extraction")
        resource.setrlimit(resource_id, (target, hard))
        observed, _ = resource.getrlimit(resource_id)
    except (AttributeError, OSError, ValueError) as error:
        raise SystemExit(f"cannot enforce the finite {label} extraction limit") from error
    if observed == resource.RLIM_INFINITY or observed > maximum:
        raise SystemExit(f"finite {label} extraction limit did not take effect")

if not hasattr(resource, "RLIMIT_CPU"):
    raise SystemExit("finite parser CPU limits are unavailable")
# Darwin exposes RLIMIT_AS through Python but the kernel rejects attempts to
# lower it with EINVAL.  The parser therefore relies on the explicit stream,
# entry, name, extension, retained-metadata, and decompressor bounds below on
# macOS.  Platforms that implement RLIMIT_AS keep the additional kernel fence.
if sys.platform != "darwin":
    if not hasattr(resource, "RLIMIT_AS"):
        raise SystemExit("finite parser address-space limits are unavailable")
    enforce_finite_soft_limit(
        resource.RLIMIT_AS,
        MAX_PARSER_ADDRESS_SPACE_BYTES,
        MIN_PARSER_ADDRESS_SPACE_BYTES,
        "parser address-space",
    )
enforce_finite_soft_limit(resource.RLIMIT_CPU, MAX_PARSER_CPU_SECONDS, 1, "parser CPU")
try:
    expected_values = tuple(int(value) for value in expected_identity.split(":"))
except ValueError as error:
    raise SystemExit("authenticated archive identity receipt is malformed") from error
if (len(expected_values) != 5 or
        ":".join(str(value) for value in expected_values) != expected_identity):
    raise SystemExit("authenticated archive identity receipt is non-canonical")

class BoundedTarInfo(tarfile.TarInfo):
    def _charge_extension(self, archive, label):
        if self.size < 0 or self.size > MAX_EXTENSION_MEMBER_BYTES:
            raise SystemExit(f"tar {label} extension header exceeds its per-member bound")
        used = getattr(archive, "_ft_extension_bytes", 0) + self.size
        if used > MAX_EXTENSION_TOTAL_BYTES:
            raise SystemExit("tar extension headers exceed their cumulative bound")
        setattr(archive, "_ft_extension_bytes", used)

    def _proc_pax(self, archive):
        self._charge_extension(archive, "PAX")
        return super()._proc_pax(archive)

    def _proc_gnulong(self, archive):
        self._charge_extension(archive, "GNU long-name")
        return super()._proc_gnulong(archive)

    def _proc_sparse(self, archive):
        raise SystemExit("tar sparse extension headers are forbidden")

class BoundedPlainReader:
    def __init__(self, source, maximum):
        self.source = source
        self.maximum = maximum
        self.total = 0
        self.closed = False

    def _charge(self, payload):
        self.total += len(payload)
        if self.total > self.maximum:
            raise SystemExit("decompressed tar stream exceeds its finite byte bound")
        return payload

    def read(self, size=-1):
        if self.closed:
            return b""
        if size is None or size < 0:
            size = MAX_IO_CHUNK_BYTES
        if size == 0:
            return b""
        size = min(size, MAX_IO_CHUNK_BYTES, self.maximum - self.total + 1)
        return self._charge(self.source.read(max(size, 1)))

    def finish(self):
        while True:
            trailing = self.read(MAX_IO_CHUNK_BYTES)
            if not trailing:
                break
            if trailing.strip(b"\0"):
                raise SystemExit("tar stream contains a nonzero post-EOT trailer")

    def close(self):
        if not self.closed:
            self.closed = True
            self.source.close()

    def __enter__(self):
        return self

    def __exit__(self, _error_type, _error, _traceback):
        self.close()

class BoundedXzReader:
    def __init__(self, source, maximum):
        self.source = source
        self.maximum = maximum
        self.total = 0
        self.closed = False
        self.finished = False
        try:
            self.decoder = lzma.LZMADecompressor(
                format=lzma.FORMAT_XZ,
                memlimit=MAX_XZ_DECODER_MEMORY_BYTES,
            )
        except (lzma.LZMAError, TypeError) as error:
            raise SystemExit("cannot establish the finite xz decoder memory limit") from error

    def _finalize_decoder(self):
        if self.decoder.unused_data or self.source.read(1):
            raise SystemExit("xz archive contains a concatenated stream or trailing bytes")
        self.finished = True

    def read(self, size=-1):
        if self.closed or self.finished:
            return b""
        if size is None or size < 0:
            size = MAX_IO_CHUNK_BYTES
        if size == 0:
            return b""
        requested = min(size, MAX_IO_CHUNK_BYTES)
        while True:
            if self.decoder.eof:
                self._finalize_decoder()
                return b""
            compressed = b""
            if self.decoder.needs_input:
                compressed = self.source.read(MAX_IO_CHUNK_BYTES)
                if not compressed:
                    raise SystemExit("xz archive ended before its decoder reached end-of-stream")
            remaining = self.maximum - self.total
            try:
                payload = self.decoder.decompress(
                    compressed,
                    max_length=max(1, min(requested, remaining + 1)),
                )
            except lzma.LZMAError as error:
                raise SystemExit(
                    "xz decoder rejected the archive memory declaration or stream"
                ) from error
            if payload:
                self.total += len(payload)
                if self.total > self.maximum:
                    raise SystemExit("decompressed tar stream exceeds its finite byte bound")
                return payload

    def finish(self):
        while True:
            trailing = self.read(MAX_IO_CHUNK_BYTES)
            if not trailing:
                break
            if trailing.strip(b"\0"):
                raise SystemExit("tar stream contains a nonzero post-EOT trailer")

    def close(self):
        if not self.closed:
            self.closed = True
            self.source.close()

    def __enter__(self):
        return self

    def __exit__(self, _error_type, _error, _traceback):
        self.close()

class BoundedZstdReader(BoundedPlainReader):
    def __init__(self, process, maximum):
        if process.stdout is None:
            raise SystemExit("bounded zstd decoder has no output descriptor")
        super().__init__(process.stdout, maximum)
        self.process = process
        self.process_finished = False

    def finish(self):
        super().finish()
        self.source.close()
        status = self.process.wait()
        self.process_finished = True
        if status != 0:
            raise SystemExit("zstd decoder rejected the font archive or its memory bound")

    def close(self):
        if not self.closed:
            self.closed = True
            self.source.close()
        if not self.process_finished:
            try:
                self.process.wait(timeout=1)
            except subprocess.TimeoutExpired:
                self.process.terminate()
                try:
                    self.process.wait(timeout=1)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait()
            self.process_finished = True

def pread_exact(fd, offset, length, label):
    payload = bytearray()
    while len(payload) < length:
        chunk = os.pread(fd, length - len(payload), offset + len(payload))
        if not chunk:
            raise SystemExit(f"zstd frame is truncated while reading {label}")
        payload.extend(chunk)
    return bytes(payload)

def require_one_canonical_zstd_frame(fd, archive_size):
    # Zstandard frame format: one standard frame, no skippable frames, no
    # concatenation, and no compressed trailer.  Parsing only framing lengths
    # keeps memory constant; zstd remains the authority for compressed blocks.
    if archive_size < 6:
        raise SystemExit("zstd archive is too short for one canonical frame")
    magic = int.from_bytes(pread_exact(fd, 0, 4, "magic"), "little")
    if 0x184D2A50 <= magic <= 0x184D2A5F:
        raise SystemExit("zstd skippable frames are forbidden")
    if magic != 0xFD2FB528:
        raise SystemExit("font archive is not one canonical zstd frame")
    offset = 4
    descriptor = pread_exact(fd, offset, 1, "frame descriptor")[0]
    offset += 1
    if descriptor & 0x18:
        raise SystemExit("zstd frame uses reserved or unused descriptor bits")
    frame_content_size_flag = descriptor >> 6
    single_segment = bool(descriptor & 0x20)
    checksum = bool(descriptor & 0x04)
    dictionary_flag = descriptor & 0x03
    if not single_segment:
        pread_exact(fd, offset, 1, "window descriptor")
        offset += 1
    dictionary_size = (0, 1, 2, 4)[dictionary_flag]
    content_size_size = (1 if single_segment else 0, 2, 4, 8)[frame_content_size_flag]
    header_tail = dictionary_size + content_size_size
    pread_exact(fd, offset, header_tail, "frame header")
    offset += header_tail
    blocks = 0
    while True:
        blocks += 1
        if blocks > MAX_ZSTD_BLOCKS:
            raise SystemExit("zstd frame exceeds its finite block-count bound")
        header = int.from_bytes(pread_exact(fd, offset, 3, "block header"), "little")
        offset += 3
        last_block = bool(header & 1)
        block_type = (header >> 1) & 0x03
        block_size = header >> 3
        if block_type == 3:
            raise SystemExit("zstd frame contains a reserved block type")
        payload_size = 1 if block_type == 1 else block_size
        if payload_size > archive_size - offset:
            raise SystemExit("zstd frame block exceeds the authenticated archive")
        offset += payload_size
        if last_block:
            break
    if checksum:
        pread_exact(fd, offset, 4, "content checksum")
        offset += 4
    if offset != archive_size:
        trailing_magic = None
        if archive_size - offset >= 4:
            trailing_magic = int.from_bytes(os.pread(fd, 4, offset), "little")
        if trailing_magic is not None and 0x184D2A50 <= trailing_magic <= 0x184D2A5F:
            raise SystemExit("zstd skippable frames are forbidden")
        raise SystemExit("zstd archive contains a concatenated frame or compressed trailer")

archive_fd = os.open(archive_path, os.O_RDONLY | nofollow | cloexec)
root_fd = -1
try:
    archive_before = os.fstat(archive_fd)
    archive_named = os.stat(archive_path, follow_symlinks=False)
    if (not stat.S_ISREG(archive_before.st_mode) or archive_before.st_nlink != 1 or
            archive_before.st_size > max_archive_bytes or
            (archive_before.st_dev, archive_before.st_ino) !=
            (archive_named.st_dev, archive_named.st_ino)):
        raise SystemExit("authenticated archive is not one bounded single-link regular file")
    archive_identity = (
        archive_before.st_dev, archive_before.st_ino, archive_before.st_size,
        archive_before.st_mtime_ns, archive_before.st_ctime_ns,
    )
    if archive_identity != expected_values:
        raise SystemExit("archive pathname no longer names the checksum-authenticated inode")
    if archive_kind == "font":
        require_one_canonical_zstd_frame(archive_fd, archive_before.st_size)

    def assert_archive_identity():
        opened = os.fstat(archive_fd)
        try:
            named = os.stat(archive_path, follow_symlinks=False)
        except OSError as error:
            raise SystemExit(
                "archive pathname no longer names the checksum-authenticated inode"
            ) from error
        current = (
            opened.st_dev, opened.st_ino, opened.st_size,
            opened.st_mtime_ns, opened.st_ctime_ns,
        )
        if (current != archive_identity or
                (opened.st_dev, opened.st_ino) != (named.st_dev, named.st_ino)):
            raise SystemExit("archive pathname no longer names the checksum-authenticated inode")

    root_before = None
    if archive_action == "extract":
        root_named = os.stat(root_path, follow_symlinks=False)
        root_fd = os.open(root_path, os.O_RDONLY | directory | nofollow | cloexec)
        root_before = os.fstat(root_fd)
        if (not stat.S_ISDIR(root_before.st_mode) or root_before.st_uid != os.geteuid() or
                stat.S_IMODE(root_before.st_mode) & 0o077 or
                (root_before.st_dev, root_before.st_ino) !=
                (root_named.st_dev, root_named.st_ino)):
            raise SystemExit("archive extraction root is not one private owner-controlled directory")
        retained_names = []
        with os.scandir(root_fd) as entries:
            for entry in entries:
                retained_names.append(entry.name)
        if archive_kind != "font" and retained_names:
            raise SystemExit("archive extraction root is not empty")
        if archive_kind == "font":
            if len(retained_names) > len(EXPECTED_FONT_FILES):
                raise SystemExit("retained font stage exceeds its exact inventory")
            for name in retained_names:
                if name not in EXPECTED_FONT_FILES:
                    raise SystemExit("retained font stage contains an unexpected entry")
                named = os.stat(name, dir_fd=root_fd, follow_symlinks=False)
                opened_fd = os.open(name, os.O_RDONLY | nofollow | cloexec, dir_fd=root_fd)
                try:
                    opened = os.fstat(opened_fd)
                    if (not stat.S_ISREG(opened.st_mode) or opened.st_uid != os.geteuid() or
                            opened.st_nlink != 1 or opened.st_size > max_expanded_bytes or
                            stat.S_IMODE(opened.st_mode) not in (0o600, 0o444) or
                            (opened.st_dev, opened.st_ino) != (named.st_dev, named.st_ino)):
                        raise SystemExit("retained font stage contains an unsafe file")
                finally:
                    os.close(opened_fd)

    def canonical_type(member):
        if member.isfile() and not member.issparse():
            return "file"
        if member.isdir():
            return "directory"
        if member.issym():
            return "symlink"
        raise SystemExit("archive contains a hard link, sparse file, or special file")

    def validate_member(member, state):
        state["entries"] += 1
        if state["entries"] > MAX_ENTRIES:
            raise SystemExit("archive exceeds its entry bound")
        name = member.name
        encoded_name = name.encode("utf-8", "surrogateescape")
        state["name_bytes"] += len(encoded_name)
        if state["name_bytes"] > MAX_NAME_BYTES:
            raise SystemExit("archive exceeds its name-byte bound")
        state["retained_metadata_bytes"] += len(encoded_name) + 256
        if state["retained_metadata_bytes"] > MAX_RETAINED_METADATA_BYTES:
            raise SystemExit("archive exceeds its retained-metadata bound")
        normalized = posixpath.normpath(name)
        if (name != normalized or name.startswith("/") or
                normalized in ("", ".", "..") or normalized.startswith("../")):
            raise SystemExit("archive contains an unsafe member name")
        if any(component.startswith("._") for component in name.split("/")):
            raise SystemExit("archive contains a forbidden AppleDouble member")
        if name in state["types"]:
            raise SystemExit("archive contains a duplicate member name")

        member_type = canonical_type(member)
        if archive_kind == "process-family":
            expected = {
                "ft", "frankenterm-mux-server", "frankenterm-pty-guardian",
                "verify-components.sh", manifest_name,
            }
            if name not in expected or member_type != "file":
                raise SystemExit("process-family archive contains an unexpected member")
        elif archive_kind == "app":
            if name == manifest_name:
                if member_type != "file":
                    raise SystemExit("detached app manifest is not one regular file")
                state["detached"] += 1
            elif name != "FrankenTerm.app" and not name.startswith("FrankenTerm.app/"):
                raise SystemExit("app archive contains an unexpected top-level member")
        elif name not in EXPECTED_FONT_FILES or member_type != "file":
            raise SystemExit("font archive contains an unexpected or non-regular member")

        linkname = ""
        if member_type == "file":
            if member.size < 0:
                raise SystemExit("archive member has a negative size")
            state["expanded_bytes"] += member.size
            if state["expanded_bytes"] > max_expanded_bytes:
                raise SystemExit("archive exceeds its expanded-byte bound")
        elif member_type == "symlink":
            linkname = member.linkname
            if len(linkname.encode("utf-8", "surrogateescape")) > MAX_LINK_BYTES:
                raise SystemExit("archive symlink target exceeds its bound")
            if posixpath.isabs(linkname):
                raise SystemExit("archive contains an absolute symlink")
            target = posixpath.normpath(posixpath.join(posixpath.dirname(name), linkname))
            if target != "FrankenTerm.app" and not target.startswith("FrankenTerm.app/"):
                raise SystemExit("app archive symlink escapes the application bundle")

        state["types"][name] = member_type
        state["sizes"][name] = member.size
        digest = state["digest"]
        for value in (name, member_type, str(member.size), linkname, str(member.mode & 0o7777)):
            encoded = value.encode("utf-8", "surrogateescape")
            digest.update(len(encoded).to_bytes(8, "big"))
            digest.update(encoded)

    def finish_scan(state):
        if not state["types"]:
            raise SystemExit("archive has an empty inventory")
        for name in state["types"]:
            parent = posixpath.dirname(name)
            while parent not in ("", "."):
                parent_type = state["types"].get(parent)
                if parent_type is not None and parent_type != "directory":
                    raise SystemExit("archive member descends through a non-directory member")
                parent = posixpath.dirname(parent)
        if archive_kind == "process-family":
            expected = {
                "ft", "frankenterm-mux-server", "frankenterm-pty-guardian",
                "verify-components.sh", manifest_name,
            }
            if set(state["types"]) != expected:
                raise SystemExit("process-family archive lacks its exact five-file inventory")
        elif archive_kind == "app" and (state["detached"] != 1 or
              state["types"].get("FrankenTerm.app") != "directory"):
            raise SystemExit("app archive lacks its exact root and detached manifest")
        elif archive_kind == "font" and set(state["types"]) != EXPECTED_FONT_FILES:
            raise SystemExit("font archive lacks its exact four-file inventory")

    def new_state():
        return {
            "entries": 0,
            "name_bytes": 0,
            "retained_metadata_bytes": 0,
            "expanded_bytes": 0,
            "detached": 0,
            "types": {},
            "sizes": {},
            "file_receipts": {},
            "digest": hashlib.sha256(),
        }

    def hash_member_payload(archive, member, state):
        source = archive.extractfile(member)
        if source is None:
            raise SystemExit("archive regular file has no readable payload")
        digest = hashlib.sha256()
        remaining = member.size
        while remaining:
            chunk = source.read(min(MAX_IO_CHUNK_BYTES, remaining))
            if not chunk:
                raise SystemExit("archive regular file is truncated")
            digest.update(chunk)
            remaining -= len(chunk)
        if source.read(1):
            raise SystemExit("archive regular file exceeds its declared size")
        state["file_receipts"][member.name] = digest.hexdigest()

    def encoded_font_receipt(state):
        if (archive_kind != "font" or
                set(state["file_receipts"]) != EXPECTED_FONT_FILES):
            raise SystemExit("font archive lacks one content receipt per exact font file")
        records = [
            {
                "bytes": state["sizes"][name],
                "path": name,
                "sha256": state["file_receipts"][name],
            }
            for name in sorted(EXPECTED_FONT_FILES)
        ]
        payload = json.dumps(records, separators=(",", ":"), sort_keys=True).encode("utf-8")
        return "FT_FONT_TREE_RECEIPT_V1=" + base64.urlsafe_b64encode(payload).decode("ascii")

    def streaming_archive():
        os.lseek(archive_fd, 0, os.SEEK_SET)
        if archive_kind == "font":
            source_fd = os.dup(archive_fd)
            try:
                process = subprocess.Popen(
                    ["zstd", "-dc", "-M64M", "-"],
                    stdin=source_fd,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.DEVNULL,
                    close_fds=True,
                )
            except (OSError, ValueError) as error:
                raise SystemExit("cannot start the bounded descriptor-fed zstd decoder") from error
            finally:
                os.close(source_fd)
            return BoundedZstdReader(process, MAX_TAR_STREAM_BYTES)

        raw = os.fdopen(os.dup(archive_fd), "rb", closefd=True)
        magic = raw.read(6)
        raw.seek(0, os.SEEK_SET)
        if magic == b"\xfd7zXZ\x00":
            return BoundedXzReader(raw, MAX_TAR_STREAM_BYTES)
        return BoundedPlainReader(raw, MAX_TAR_STREAM_BYTES)

    first = new_state()
    assert_archive_identity()
    with streaming_archive() as raw:
        with tarfile.open(
                fileobj=raw, mode="r|", bufsize=512, tarinfo=BoundedTarInfo) as archive:
            for member in archive:
                validate_member(member, first)
                if archive_kind == "font" and first["types"][member.name] == "file":
                    hash_member_payload(archive, member, first)
                if len(archive.members) > 1:
                    raise SystemExit("tar parser retained more than one streamed member")
                archive.members.clear()
        raw.finish()
    finish_scan(first)
    first_fingerprint = first["digest"].digest()
    assert_archive_identity()
    if archive_action == "scan":
        print(encoded_font_receipt(first))
        raise SystemExit(0)
    # Both pass inventories coexist until the extraction comparison completes.
    # Refuse before writing anything unless two equal inventories fit within the
    # one aggregate retained-metadata budget.
    if first["retained_metadata_bytes"] > MAX_RETAINED_METADATA_BYTES // 2:
        raise SystemExit("archive exceeds its aggregate two-pass retained-metadata bound")
    filesystem = os.fstatvfs(root_fd)
    available = filesystem.f_bavail * filesystem.f_frsize
    required = first["expanded_bytes"] + free_space_headroom
    if available < required:
        raise SystemExit(
            f"archive extraction requires {required} free bytes but only {available} are available"
        )

    def open_parent(name):
        parts = name.split("/")
        parent_fd = os.dup(root_fd)
        prefix = []
        try:
            for component in parts[:-1]:
                prefix.append(component)
                try:
                    os.mkdir(component, 0o700, dir_fd=parent_fd)
                    os.fsync(parent_fd)
                except FileExistsError:
                    pass
                named = os.stat(component, dir_fd=parent_fd, follow_symlinks=False)
                child_fd = os.open(
                    component, os.O_RDONLY | directory | nofollow | cloexec,
                    dir_fd=parent_fd,
                )
                opened = os.fstat(child_fd)
                if (not stat.S_ISDIR(opened.st_mode) or opened.st_uid != os.geteuid() or
                        (opened.st_dev, opened.st_ino) != (named.st_dev, named.st_ino)):
                    os.close(child_fd)
                    raise SystemExit("archive extraction parent changed type or identity")
                os.close(parent_fd)
                parent_fd = child_fd
            return parent_fd, parts[-1]
        except BaseException:
            os.close(parent_fd)
            raise

    def extract_member(archive, member, member_type):
        parent_fd, leaf = open_parent(member.name)
        output_fd = -1
        content_digest = None
        try:
            if member_type == "directory":
                try:
                    os.mkdir(leaf, 0o700, dir_fd=parent_fd)
                    os.fsync(parent_fd)
                except FileExistsError:
                    pass
                named = os.stat(leaf, dir_fd=parent_fd, follow_symlinks=False)
                output_fd = os.open(
                    leaf, os.O_RDONLY | directory | nofollow | cloexec, dir_fd=parent_fd)
                opened = os.fstat(output_fd)
                if (not stat.S_ISDIR(opened.st_mode) or opened.st_uid != os.geteuid() or
                        (opened.st_dev, opened.st_ino) != (named.st_dev, named.st_ino)):
                    raise SystemExit("archive directory changed type or identity")
                os.fchmod(output_fd, 0o700)
                os.fsync(output_fd)
            elif member_type == "symlink":
                os.symlink(member.linkname, leaf, dir_fd=parent_fd)
                os.fsync(parent_fd)
            else:
                source = archive.extractfile(member)
                if source is None:
                    raise SystemExit("archive regular file has no readable payload")
                if archive_kind == "font":
                    try:
                        output_fd = os.open(
                            leaf,
                            os.O_RDWR | os.O_CREAT | os.O_EXCL | nofollow | cloexec,
                            0o600,
                            dir_fd=parent_fd,
                        )
                        os.fsync(parent_fd)
                    except FileExistsError:
                        output_fd = os.open(
                            leaf, os.O_RDONLY | nofollow | cloexec, dir_fd=parent_fd)
                        retained = os.fstat(output_fd)
                        named = os.stat(leaf, dir_fd=parent_fd, follow_symlinks=False)
                        retained_mode = stat.S_IMODE(retained.st_mode)
                        if (not stat.S_ISREG(retained.st_mode) or
                                retained.st_uid != os.geteuid() or retained.st_nlink != 1 or
                                retained.st_size > member.size or
                                retained_mode not in (0o600, 0o444) or
                                (retained.st_dev, retained.st_ino) !=
                                (named.st_dev, named.st_ino) or
                                (retained_mode == 0o444 and retained.st_size != member.size)):
                            raise SystemExit("retained font file is not one safe exact prefix")
                        if retained_mode == 0o600:
                            os.close(output_fd)
                            output_fd = os.open(
                                leaf, os.O_RDWR | nofollow | cloexec, dir_fd=parent_fd)
                            reopened = os.fstat(output_fd)
                            named_reopened = os.stat(
                                leaf, dir_fd=parent_fd, follow_symlinks=False)
                            if (not stat.S_ISREG(reopened.st_mode) or
                                    reopened.st_uid != os.geteuid() or reopened.st_nlink != 1 or
                                    stat.S_IMODE(reopened.st_mode) != 0o600 or
                                    (reopened.st_dev, reopened.st_ino, reopened.st_size,
                                     reopened.st_mtime_ns, reopened.st_ctime_ns) !=
                                    (retained.st_dev, retained.st_ino, retained.st_size,
                                     retained.st_mtime_ns, retained.st_ctime_ns) or
                                    (reopened.st_dev, reopened.st_ino) !=
                                    (named_reopened.st_dev, named_reopened.st_ino)):
                                raise SystemExit("retained font file changed while reopening")
                else:
                    output_fd = os.open(
                        leaf,
                        os.O_WRONLY | os.O_CREAT | os.O_EXCL | nofollow | cloexec,
                        0o600,
                        dir_fd=parent_fd,
                    )
                target_before = os.fstat(output_fd)
                existing_size = target_before.st_size if archive_kind == "font" else 0
                os.lseek(output_fd, 0, os.SEEK_SET)
                digest = hashlib.sha256()
                remaining = member.size
                offset = 0
                while remaining:
                    chunk = source.read(min(MAX_IO_CHUNK_BYTES, remaining))
                    if not chunk:
                        raise SystemExit("archive regular file is truncated")
                    digest.update(chunk)
                    retained_width = min(len(chunk), max(0, existing_size - offset))
                    if retained_width:
                        retained_chunk = os.read(output_fd, retained_width)
                        if retained_chunk != chunk[:retained_width]:
                            raise SystemExit("retained font file is not an exact archive prefix")
                    view = memoryview(chunk)[retained_width:]
                    while view:
                        written = os.write(output_fd, view)
                        if written <= 0:
                            raise SystemExit("archive extraction write made no progress")
                        view = view[written:]
                    offset += len(chunk)
                    remaining -= len(chunk)
                if source.read(1):
                    raise SystemExit("archive regular file exceeds its declared size")
                content_digest = digest.hexdigest()
                if archive_kind == "font":
                    os.fsync(output_fd)
                    target_before_readback = os.fstat(output_fd)
                    os.lseek(output_fd, 0, os.SEEK_SET)
                    retained_digest = hashlib.sha256()
                    retained_bytes = 0
                    while True:
                        chunk = os.read(output_fd, MAX_IO_CHUNK_BYTES)
                        if not chunk:
                            break
                        retained_bytes += len(chunk)
                        if retained_bytes > member.size:
                            raise SystemExit("completed font file has an unexpected suffix")
                        retained_digest.update(chunk)
                    target_after_readback = os.fstat(output_fd)
                    named_after = os.stat(leaf, dir_fd=parent_fd, follow_symlinks=False)
                    if (retained_bytes != member.size or
                            retained_digest.hexdigest() != content_digest or
                            (target_before_readback.st_dev, target_before_readback.st_ino,
                             target_before_readback.st_size, target_before_readback.st_mtime_ns,
                             target_before_readback.st_ctime_ns) !=
                            (target_after_readback.st_dev, target_after_readback.st_ino,
                             target_after_readback.st_size, target_after_readback.st_mtime_ns,
                             target_after_readback.st_ctime_ns) or
                            (target_after_readback.st_dev, target_after_readback.st_ino) !=
                            (named_after.st_dev, named_after.st_ino)):
                        raise SystemExit("completed font file failed exact descriptor readback")
                    if stat.S_IMODE(target_after_readback.st_mode) != 0o444:
                        os.fchmod(output_fd, 0o444)
                        os.fsync(output_fd)
                else:
                    os.fchmod(output_fd, 0o555 if member.mode & 0o111 else 0o444)
                    os.fsync(output_fd)
                os.fsync(parent_fd)
        finally:
            if output_fd >= 0:
                os.close(output_fd)
            os.close(parent_fd)
        return content_digest

    second = new_state()
    assert_archive_identity()
    with streaming_archive() as raw:
        with tarfile.open(
                fileobj=raw, mode="r|", bufsize=512, tarinfo=BoundedTarInfo) as archive:
            for member in archive:
                validate_member(member, second)
                expected_type = first["types"].get(member.name)
                if expected_type != second["types"][member.name]:
                    raise SystemExit("archive metadata changed between bounded passes")
                content_digest = extract_member(archive, member, expected_type)
                if archive_kind == "font":
                    second["file_receipts"][member.name] = content_digest
                if len(archive.members) > 1:
                    raise SystemExit("tar parser retained more than one streamed member")
                archive.members.clear()
        raw.finish()
    finish_scan(second)
    if (second["types"] != first["types"] or
            second["sizes"] != first["sizes"] or
            second["digest"].digest() != first_fingerprint or
            (archive_kind == "font" and
             second["file_receipts"] != first["file_receipts"])):
        raise SystemExit("archive inventory changed between bounded passes")

    assert_archive_identity()
    root_after = os.fstat(root_fd)
    root_named_after = os.stat(root_path, follow_symlinks=False)
    if (root_before.st_dev, root_before.st_ino) != (root_after.st_dev, root_after.st_ino) or (
            root_after.st_dev, root_after.st_ino) != (
            root_named_after.st_dev, root_named_after.st_ino):
        raise SystemExit("archive extraction root changed during extraction")
    os.fsync(root_fd)
    if archive_kind == "font":
        print(encoded_font_receipt(second))
finally:
    if root_fd >= 0:
        os.close(root_fd)
    os.close(archive_fd)
PY
}

verify_minisign_signature() {
  local file="$1" artifact_url="$2" expected_identity="${3:-$VERIFIED_ARCHIVE_IDENTITY}"
  local signature_override="${4:-}"
  local signature_source="${5:-}"
  local minisign_path minisign_timeout=120 signature_url signature_file
  if ! command -v minisign >/dev/null 2>&1; then
    err "minisign is required to verify DSR release artifacts"
    return 1
  fi
  signature_url="$signature_override"
  signature_file="$TMP/$(basename "$file").minisig"
  if [ -n "$signature_source" ]; then
    if ! ensure_exact_staged_file "$signature_source" "$signature_file" 0400; then
      err "Required offline DSR minisign signature is unsafe or unreadable: $signature_source"
      return 1
    fi
  else
    [ -z "$signature_url" ] && signature_url="${artifact_url}.minisig"
    if [ -z "$artifact_url" ] || \
       ! download_https_bounded "$signature_url" "$signature_file" 65536 30 2>/dev/null; then
      err "Required DSR minisign signature not found at $signature_url"
      return 1
    fi
  fi
  [ -n "$expected_identity" ] || {
    err "Minisign verification lacks the checksum-authenticated archive identity"
    return 1
  }
  minisign_path=$(command -v minisign) || return 1
  if [ "${FT_INSTALL_TEST_LIBRARY_ONLY:-0}" = 1 ] &&
      [ "${FT_INSTALL_TEST_ENABLE_RESOURCE_OVERRIDES:-0}" = 1 ] &&
      [ -n "${FT_INSTALL_TEST_MINISIGN_TIMEOUT_SECONDS:-}" ]; then
    minisign_timeout="$FT_INSTALL_TEST_MINISIGN_TIMEOUT_SECONDS"
  fi
  [[ "$minisign_timeout" =~ ^[1-9][0-9]*$ ]] || return 1
  # Both inputs reach minisign through inherited descriptors. Reopening either
  # pathname would let a same-UID replacement make the checksum, signature,
  # and extractor authenticate different bytes.
  if ! python3 - "$file" "$expected_identity" "$signature_file" "$minisign_path" \
      "$MINISIGN_PUBLIC_KEY" "$minisign_timeout" <<'PY'
import os, resource, signal, stat, subprocess, sys

archive_path, expected_raw, signature_path, minisign, public_key, timeout_text = sys.argv[1:]
timeout_seconds = int(timeout_text)
try:
    expected = tuple(int(value) for value in expected_raw.split(":"))
except ValueError as error:
    raise SystemExit("checksum-authenticated archive identity is malformed") from error
if len(expected) != 5 or ":".join(str(value) for value in expected) != expected_raw:
    raise SystemExit("checksum-authenticated archive identity is non-canonical")

flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0)
archive_fd = signature_fd = -1
try:
    archive_fd = os.open(archive_path, flags)
    signature_fd = os.open(signature_path, flags)
    archive_before = os.fstat(archive_fd)
    signature_before = os.fstat(signature_fd)
    archive_named = os.stat(archive_path, follow_symlinks=False)
    signature_named = os.stat(signature_path, follow_symlinks=False)
    archive_observed = (
        archive_before.st_dev, archive_before.st_ino, archive_before.st_size,
        archive_before.st_mtime_ns, archive_before.st_ctime_ns,
    )
    if (not stat.S_ISREG(archive_before.st_mode) or archive_before.st_nlink != 1 or
            archive_observed != expected or
            (archive_before.st_dev, archive_before.st_ino) !=
            (archive_named.st_dev, archive_named.st_ino)):
        raise SystemExit("minisign archive is not the checksum-authenticated inode")
    if (not stat.S_ISREG(signature_before.st_mode) or signature_before.st_nlink != 1 or
            signature_before.st_size > 64 * 1024 or
            (signature_before.st_dev, signature_before.st_ino) !=
            (signature_named.st_dev, signature_named.st_ino)):
        raise SystemExit("minisign signature is not one bounded single-link regular file")

    archive_descriptor = f"/dev/fd/{archive_fd}"
    signature_descriptor = f"/dev/fd/{signature_fd}"
    if not os.path.exists(archive_descriptor) or not os.path.exists(signature_descriptor):
        raise SystemExit("descriptor-backed minisign verification is unavailable")

    def constrain_verifier_child():
        def finite_limit(resource_id, maximum, minimum):
            soft, hard = resource.getrlimit(resource_id)
            candidates = [maximum]
            if soft != resource.RLIM_INFINITY:
                candidates.append(soft)
            if hard != resource.RLIM_INFINITY:
                candidates.append(hard)
            target = min(candidates)
            if target < minimum:
                raise OSError("inherited verifier resource limit is too small")
            resource.setrlimit(resource_id, (target, hard))
        finite_limit(resource.RLIMIT_CPU, timeout_seconds + 5, 1)
        if sys.platform != "darwin" and hasattr(resource, "RLIMIT_AS"):
            finite_limit(resource.RLIMIT_AS, 4 * 1024 * 1024 * 1024, 256 * 1024 * 1024)
        if hasattr(resource, "RLIMIT_FSIZE"):
            finite_limit(resource.RLIMIT_FSIZE, 64 * 1024 * 1024, 1024 * 1024)
        if hasattr(resource, "RLIMIT_NOFILE"):
            finite_limit(resource.RLIMIT_NOFILE, 256, 32)

    verifier = None
    try:
        verifier = subprocess.Popen(
            [
                minisign, "-Vm", archive_descriptor, "-x", signature_descriptor,
                "-P", public_key,
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            close_fds=True,
            pass_fds=(archive_fd, signature_fd),
            preexec_fn=constrain_verifier_child,
            start_new_session=True,
        )
        try:
            returncode = verifier.wait(timeout=timeout_seconds)
        except subprocess.TimeoutExpired as error:
            try:
                os.killpg(verifier.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            verifier.wait()
            raise SystemExit(
                "minisign verifier exceeded its finite wall-clock bound"
            ) from error
    except (OSError, subprocess.SubprocessError) as error:
        if verifier is not None and verifier.poll() is None:
            try:
                os.killpg(verifier.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            verifier.wait()
        raise SystemExit(
            "minisign verifier could not establish its child resource contract"
        ) from error
    archive_after = os.fstat(archive_fd)
    signature_after = os.fstat(signature_fd)
    archive_named_after = os.stat(archive_path, follow_symlinks=False)
    signature_named_after = os.stat(signature_path, follow_symlinks=False)
    if archive_observed != (
            archive_after.st_dev, archive_after.st_ino, archive_after.st_size,
            archive_after.st_mtime_ns, archive_after.st_ctime_ns,
    ) or (archive_after.st_dev, archive_after.st_ino) != (
            archive_named_after.st_dev, archive_named_after.st_ino,
    ):
        raise SystemExit("checksum-authenticated archive changed during minisign verification")
    if (signature_before.st_dev, signature_before.st_ino, signature_before.st_size,
        signature_before.st_mtime_ns, signature_before.st_ctime_ns) != (
            signature_after.st_dev, signature_after.st_ino, signature_after.st_size,
            signature_after.st_mtime_ns, signature_after.st_ctime_ns,
    ) or (signature_after.st_dev, signature_after.st_ino) != (
            signature_named_after.st_dev, signature_named_after.st_ino,
    ):
        raise SystemExit("minisign signature changed during verification")
    raise SystemExit(returncode)
finally:
    if signature_fd >= 0:
        os.close(signature_fd)
    if archive_fd >= 0:
        os.close(archive_fd)
PY
  then
    return 1
  fi
  ok "Signature verified (DSR minisign key 69B3955C8D2E62A8)"
  return 0
}

require_release_minisign() {
  if [ "$FROM_SOURCE" -eq 1 ] || [ "$NO_MINISIGN" -eq 1 ]; then
    return 0
  fi
  if command -v minisign >/dev/null 2>&1; then
    return 0
  fi
  err "Required tool not found: minisign"
  err "DSR release verification is mandatory before any release artifact download."
  err "Install minisign via your package manager:"
  err "  macOS:        brew install minisign"
  err "  Debian/Ubuntu: sudo apt-get install -y minisign"
  err "  RHEL/Fedora:   sudo dnf install -y minisign"
  err "  Alpine:        sudo apk add minisign"
  err "Use --no-verify only for an explicitly trusted local test artifact; SHA-256 remains mandatory."
  return 1
}

# ───────────────────────────────────────────────────────────────────────────
# Optional: Pragmasevka Nerd Font install
# ───────────────────────────────────────────────────────────────────────────
install_pragmasevka() {
  FONT_INSTALLED_PATH=""
  # --offline promises no network; honour that for the font too.
  if [ -n "$OFFLINE_TARBALL" ]; then
    warn "Skipping --with-font in --offline mode (no network)."
    warn "Install the font manually from your distro / Homebrew if needed."
    return 0
  fi
  # The process-family manifest is inside the checksum-authenticated release
  # archive and records the exact SHA-256 and byte count of this repository
  # payload. That immutable generation receipt, not the mutable download URL,
  # is the authority for the optional font archive.
  local font_url="https://raw.githubusercontent.com/${OWNER}/${REPO}/${VERSION}/crates/frankenterm/assets/Pragmasevka_NF.zip.zst"
  local font_dir="" font_parent="" font_stage="" font_stage_name=""
  local font_manifest="$PUBLISHED_PROCESS_FAMILY_ROOT/process-family.component-manifest.json"
  local font_receipt="" font_checksum="" font_bytes="" font_identity=""
  local downloaded_bytes="" tree_receipt="" extracted_receipt=""
  local helper="" stage_id="" target_id="" txid="" operation=""
  case "$OS" in
    linux)  font_dir="${XDG_DATA_HOME:-$HOME/.local/share}/fonts/pragmasevka" ;;
    darwin) font_dir="$HOME/Library/Fonts/pragmasevka" ;;
    *)      warn "Unknown OS for font install; skipping"; return 0 ;;
  esac
  command -v zstd >/dev/null 2>&1 || { warn "zstd not found; skipping font install (install with: brew install zstd | apt install zstd)"; return 0; }
  if [ -z "$PUBLISHED_PROCESS_FAMILY_ROOT" ] ||
      [ -z "$PUBLISHED_PROCESS_FAMILY_VERSION" ] ||
      [ -z "$PUBLISHED_PROCESS_FAMILY_VERIFIER_AUTHORITY" ] ||
      ! verify_canonical_generation "$PUBLISHED_PROCESS_FAMILY_ROOT" \
        "$PUBLISHED_PROCESS_FAMILY_VERSION" \
        "$PUBLISHED_PROCESS_FAMILY_VERIFIER_AUTHORITY"; then
    warn "Authenticated process-family receipt unavailable; skipping font install"
    return 0
  fi
  font_receipt=$(process_family_input_receipt \
    "$font_manifest" \
    font.payload \
    crates/frankenterm/assets/Pragmasevka_NF.zip.zst \
    "$MAX_FONT_ARCHIVE_BYTES" \
    "${PUBLISHED_PROCESS_FAMILY_ROOT##*/}") || {
      warn "Authenticated font payload receipt is invalid; skipping font install"
      return 0
    }
  IFS=$'\t' read -r font_checksum font_bytes <<<"$font_receipt"
  [[ "$font_checksum" =~ ^[0-9a-f]{64}$ ]] && [[ "$font_bytes" =~ ^[0-9]+$ ]] || {
    warn "Authenticated font payload receipt is malformed; skipping font install"
    return 0
  }

  font_parent="${font_dir%/*}"
  # Font installation is best-effort, but every mutation remains staged and
  # exact. A failure can skip the font; it cannot weaken the ft installation.
  if ! mkdir -p "$font_parent" 2>/dev/null; then
    warn "Could not create font parent $font_parent; skipping font install"
    return 0
  fi
  if ! require_transfer_capacity "$TMP" "$font_parent" \
      "$MAX_FONT_ARCHIVE_BYTES" "$MAX_FONT_EXPANDED_BYTES" \
      "Pragmasevka font"; then
    warn "Insufficient bounded capacity for Pragmasevka; skipping font install"
    return 0
  fi
  info "Fetching Pragmasevka NF from $font_url"
  if ! download_https_bounded "$font_url" "$TMP/pragmasevka.zip.zst" \
      "$MAX_FONT_ARCHIVE_BYTES" 60 1; then
    warn "Pragmasevka payload download failed; skipping font install"
    return 0
  fi
  if ! verify_checksum "$TMP/pragmasevka.zip.zst" "$font_checksum"; then
    warn "Pragmasevka payload authentication failed; skipping font install"
    return 0
  fi
  font_identity="$VERIFIED_ARCHIVE_IDENTITY"
  downloaded_bytes="${font_identity#*:}"
  downloaded_bytes="${downloaded_bytes#*:}"
  downloaded_bytes="${downloaded_bytes%%:*}"
  if [ "$downloaded_bytes" != "$font_bytes" ]; then
    warn "Pragmasevka payload size differs from its authenticated receipt; skipping"
    return 0
  fi
  tree_receipt=$(extract_authenticated_archive \
    "$TMP/pragmasevka.zip.zst" "$font_parent" font - "$font_identity" scan) || {
      warn "Pragmasevka payload failed authenticated receipt derivation; skipping"
      return 0
    }
  [[ "$tree_receipt" == FT_FONT_TREE_RECEIPT_V1=* ]] &&
      [[ "$tree_receipt" != *$'\n'* ]] || {
    warn "Pragmasevka payload returned a malformed tree receipt; skipping"
    return 0
  }

  if [ -d "$font_dir" ] && [ ! -L "$font_dir" ] &&
      verify_font_tree_receipt "$font_dir" "$tree_receipt" 1; then
    ok "Pragmasevka NF already matches its authenticated generation at $font_dir"
    FONT_INSTALLED_PATH="$font_dir"
    return 0
  fi
  if { [ -e "$font_dir" ] || [ -L "$font_dir" ]; } &&
      { [ ! -d "$font_dir" ] || [ -L "$font_dir" ]; }; then
    warn "Refusing to replace non-directory or symlink font target at $font_dir"
    return 0
  fi

  font_stage_name=$(select_font_generation_stage \
    "$font_parent" "$font_checksum" "$tree_receipt") || {
      warn "No bounded private Pragmasevka generation stage is available; skipping"
      return 0
    }
  font_stage="$font_parent/$font_stage_name"
  if ! prepare_font_generation_stage "$font_parent" "$font_stage_name"; then
    warn "Could not prepare a private resumable Pragmasevka generation; skipping"
    return 0
  fi
  extracted_receipt=$(extract_authenticated_archive \
    "$TMP/pragmasevka.zip.zst" "$font_stage" font - "$font_identity" extract) || {
      warn "Pragmasevka payload failed bounded authenticated extraction; skipping"
      return 0
    }
  if [ "$extracted_receipt" != "$tree_receipt" ]; then
    warn "Pragmasevka extraction receipt differs from its authenticated scan; skipping"
    return 0
  fi
  installer_failpoint after-font-extraction
  installer_test_mutate_font_stage "$font_stage" || return 0
  if ! seal_font_generation_stage "$font_stage" "$tree_receipt" ||
      ! fsync_installer_tree "$font_stage" ||
      ! verify_font_tree_receipt "$font_stage" "$tree_receipt" 1; then
    warn "Pragmasevka generation failed exact receipt sealing; skipping"
    return 0
  fi

  helper="$PUBLISHED_PROCESS_FAMILY_ROOT/ft"
  stage_id=$(atomic_path_content_id "$helper" "$font_parent" "$font_stage_name") || {
    warn "Pragmasevka generation lacks one sealed atomic content identity; skipping"
    return 0
  }
  # This second receipt check followed by the atomic helper's stage-content-ID
  # check closes the final same-UID mutation window before the namespace move.
  verify_font_tree_receipt "$font_stage" "$tree_receipt" 1 || return 0
  if [ -e "$font_dir" ]; then
    [ -d "$font_dir" ] && [ ! -L "$font_dir" ] || return 0
    target_id=$(atomic_path_content_id "$helper" "$font_parent" "$(basename "$font_dir")") || return 0
    operation=exchange
  else
    target_id=missing
    operation=publish-noreplace
  fi
  txid=$(atomic_transition_txid \
    "font-generation:$font_parent:$font_stage_name:$font_checksum:$target_id") || return 0
  installer_failpoint before-font-publication
  if ! atomic_path_transition "$helper" "$font_parent" "$font_stage_name" \
      "$(basename "$font_dir")" "$txid" "$stage_id" "$target_id" "$operation"; then
    warn "Pragmasevka atomic generation publication failed; prior font tree was retained"
    return 0
  fi
  installer_failpoint after-font-publication
  if ! verify_font_tree_receipt "$font_dir" "$tree_receipt" 1; then
    warn "Published Pragmasevka generation failed its exact post-publication receipt"
    [ "$operation" = exchange ] &&
      warn "The prior font generation remains retained at $font_stage"
    return 0
  fi
  ok "Pragmasevka NF installed to $font_dir"
  if [ "$operation" = exchange ]; then
    info "Previous Pragmasevka generation preserved at $font_stage"
  fi
  FONT_INSTALLED_PATH="$font_dir"
  if [ "$OS" = "linux" ] && command -v fc-cache >/dev/null 2>&1; then
    if run_with_spinner "Refreshing font cache" fc-cache -f "$font_dir"; then
      ok "Font cache refreshed"
    else
      warn "Font files installed, but font cache refresh failed"
    fi
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
  if [ "$INSTALL_APP" -eq 0 ]; then
    mark_app_not_selected explicit_opt_out
    return 1
  fi
  # GUI app is macOS-only.
  if [ "${OS:-}" != "darwin" ]; then
    if [ "$INSTALL_APP" -eq 1 ]; then
      APP_RECEIPT_REQUESTED="true"
      mark_app_skipped unsupported_platform
      warn "--with-app ignored: the FrankenTerm GUI app is macOS-only"
    else
      mark_app_not_selected automatic_platform_exclusion
    fi
    return 1
  fi
  # Only the arm64 prebuilt bundle is published.
  if [ "${ARCH:-}" != "aarch64" ]; then
    if [ "$INSTALL_APP" -eq 1 ]; then
      APP_RECEIPT_REQUESTED="true"
      mark_app_skipped unsupported_architecture
      warn "--with-app ignored: no prebuilt FrankenTerm.app for ${OS}/${ARCH}; build it with scripts/create-macos-bundle.sh"
    else
      mark_app_not_selected automatic_architecture_exclusion
    fi
    return 1
  fi
  # Source builds and offline mode have no published .app to fetch.
  if [ "$FROM_SOURCE" -eq 1 ]; then
    if [ "$INSTALL_APP" -eq 1 ]; then
      APP_RECEIPT_REQUESTED="true"
      mark_app_skipped source_build_has_no_app_asset
      warn "--with-app ignored for source builds; run scripts/create-macos-bundle.sh after building"
    else
      mark_app_not_selected automatic_source_build_exclusion
    fi
    return 1
  fi
  if [ -n "$OFFLINE_TARBALL" ]; then
    if [ "$INSTALL_APP" -eq 1 ]; then
      APP_RECEIPT_REQUESTED="true"
      mark_app_skipped offline_mode_has_no_app_asset
      warn "--with-app ignored in --offline mode (no network for the .app asset)"
    else
      mark_app_not_selected automatic_offline_exclusion
    fi
    return 1
  fi
  APP_RECEIPT_REQUESTED="true"
  APP_RECEIPT_RESULT="in_progress"
  APP_RECEIPT_REASON="selected"
  APP_RECEIPT_MANIFEST_ID=""
  APP_RECEIPT_CANDIDATE_PATH=""
  APP_RECEIPT_READINESS="not_run"
  APP_ACTIVATION_STATE="none"
  return 0
}

install_macos_app() {
  local app_url dest tmp_app_tar app_archive_identity extraction_root extracted_app app_manifest
  local target_app staged_app app_metadata standalone_metadata app_manifest_id app_id
  local app_build app_source app_version app_target app_profile app_features
  local app_inventory_bytes _family_manifest_id family_build family_source family_version
  local family_target family_profile family_features family_inventory_bytes
  local stage_id target_id txid operation retained_manifest manifest_store manifest_stage
  local family_manifest family_verifier transition_helper
  app_url="https://github.com/${OWNER}/${REPO}/releases/download/${VERSION}/${APP_ASSET}"

  # Bind the app to the externally authenticated generation published by this
  # exact installer transaction, not only to the pre-existing live selector.
  # Fresh installs and upgrades intentionally leave that candidate pending, so
  # requiring ACTIVE_* here would silently skip app staging on every safe
  # non-activating path.
  family_manifest="$PUBLISHED_PROCESS_FAMILY_ROOT/process-family.component-manifest.json"
  family_verifier="$PUBLISHED_PROCESS_FAMILY_VERIFIER_AUTHORITY"
  transition_helper="$PUBLISHED_PROCESS_FAMILY_ROOT/ft"
  if [ -z "$PUBLISHED_PROCESS_FAMILY_ROOT" ] || \
     [ -z "$PUBLISHED_PROCESS_FAMILY_VERSION" ] || \
     [ -z "$family_verifier" ] || [ ! -x "$transition_helper" ] || \
     ! verify_canonical_generation "$PUBLISHED_PROCESS_FAMILY_ROOT" \
       "$PUBLISHED_PROCESS_FAMILY_VERSION" "$family_verifier"; then
    warn "No externally authenticated published process-family authority is available; skipping GUI app"
    mark_app_skipped published_family_authority_unavailable
    return 0
  fi

  # Atomic rename authority is descriptor-pinned and refuses group/world
  # writable parents. Prefer /Applications only when this process owns that
  # exact private directory; otherwise use the user's first-class app folder.
  dest="$APP_DEST"
  if [ -z "$dest" ]; then
    dest="/Applications"
    if ! python3 - "$dest" <<'PY' >/dev/null 2>&1
import os, stat, sys
s = os.lstat(sys.argv[1])
raise SystemExit(0 if stat.S_ISDIR(s.st_mode) and not stat.S_ISLNK(s.st_mode)
                 and s.st_uid == os.geteuid() and not (s.st_mode & 0o022) else 1)
PY
    then
      dest="$HOME/Applications"
      info "Using private atomic app destination $dest"
    fi
  fi
  if ! mkdir -p "$dest" 2>/dev/null; then
    warn "Could not create app destination $dest; skipping GUI app install"
    mark_app_skipped destination_creation_failed
    return 0
  fi
  if ! python3 - "$dest" <<'PY'
import os, stat, sys
s = os.lstat(sys.argv[1])
if (not stat.S_ISDIR(s.st_mode) or stat.S_ISLNK(s.st_mode) or
        s.st_uid != os.geteuid() or s.st_mode & 0o022):
    raise SystemExit("app destination is not one private owner-controlled directory")
PY
  then
    warn "App destination cannot provide descriptor-pinned atomic authority: $dest"
    mark_app_skipped unsafe_destination_authority
    return 0
  fi
  if ! require_transfer_capacity "$TMP" "$dest" \
      "$MAX_APP_ARCHIVE_BYTES" "$MAX_APP_EXPANDED_BYTES" \
      "FrankenTerm.app"; then
    warn "Insufficient bounded capacity for FrankenTerm.app; skipping GUI app install"
    mark_app_skipped insufficient_destination_capacity
    return 0
  fi

  info "Downloading FrankenTerm.app from $app_url"
  tmp_app_tar="$TMP/$APP_ASSET"
  if ! run_with_spinner "Downloading $APP_ASSET" \
      download_https_bounded "$app_url" "$tmp_app_tar" \
        "$MAX_APP_ARCHIVE_BYTES" 300 1; then
    warn "FrankenTerm.app asset not found at $app_url; skipping GUI app install"
    mark_app_skipped app_asset_download_failed
    return 0
  fi

  # The detached manifest and verifier are meaningful only when rooted in the
  # externally fetched release archive checksum. DSR minisign verification may
  # be explicitly disabled, but SHA-256 authentication is never bypassed.
  local app_sum=""
  if download_https_bounded "${app_url}.sha256" "$TMP/app.sha256" 4096 30 \
      2>/dev/null; then
    app_sum=$(read_sha256_sidecar "$TMP/app.sha256" "$APP_ASSET" 2>/dev/null || true)
  fi
  if [ -z "$app_sum" ] || ! verify_checksum "$tmp_app_tar" "$app_sum"; then
    warn "FrankenTerm.app checksum is absent or invalid; skipping GUI app install"
    mark_app_skipped app_checksum_invalid
    return 0
  fi
  app_archive_identity="$VERIFIED_ARCHIVE_IDENTITY"
  if [ "$NO_MINISIGN" -eq 0 ]; then
    if ! verify_minisign_signature "$tmp_app_tar" "$app_url" \
        "$app_archive_identity" "$APP_MINISIGN_SIGNATURE_URL"; then
      warn "FrankenTerm.app DSR minisign verification failed; live app authority is unchanged"
      mark_app_skipped app_minisign_invalid
      return 0
    fi
  else
    warn "FrankenTerm.app DSR minisign verification skipped (--no-verify); SHA-256 was still verified"
  fi

  # Validate the complete archive namespace before extracting into a new
  # private directory. No member may traverse out of the two expected roots or
  # descend through an archived symlink; hard links and special files are
  # forbidden. This makes the outer checksum an authority over exact bytes,
  # not permission to let tar interpret an attacker-controlled namespace.
  extraction_root="$TMP/app-package"
  mkdir -m 0700 "$extraction_root" || {
    mark_app_skipped extraction_root_creation_failed
    return 0
  }
  if ! extract_authenticated_archive "$tmp_app_tar" "$extraction_root" app \
      FrankenTerm.app.component-manifest.json "$app_archive_identity"
  then
    warn "FrankenTerm.app archive namespace failed validation; skipping GUI app install"
    mark_app_skipped app_archive_invalid
    return 0
  fi
  extracted_app="$extraction_root/FrankenTerm.app"
  app_manifest="$extraction_root/FrankenTerm.app.component-manifest.json"
  [ -d "$extracted_app" ] && [ ! -L "$extracted_app" ] && \
    [ -f "$app_manifest" ] && [ ! -L "$app_manifest" ] || {
    mark_app_skipped app_archive_incomplete
    return 0
  }

  # The verifier authority comes from the independently checksummed standalone
  # package. It re-hashes the complete app tree, including the app's shipped
  # verifier, and then the two detached manifests must bind one exact release.
  bash "$family_verifier" verify \
    --root "$extracted_app" --manifest "$app_manifest" >/dev/null || {
    warn "Detached app component verification failed; skipping GUI app install"
    mark_app_skipped app_component_verification_failed
    return 0
    }
  app_metadata=$(process_family_manifest_metadata "$app_manifest" app) || return 0
  standalone_metadata=$(process_family_manifest_metadata "$family_manifest" triplet) || return 0
  IFS=$'\t' read -r app_manifest_id app_build app_source app_version app_target app_profile app_features app_inventory_bytes <<<"$app_metadata"
  IFS=$'\t' read -r _family_manifest_id family_build family_source family_version family_target family_profile family_features family_inventory_bytes <<<"$standalone_metadata"
  [[ "$app_inventory_bytes" =~ ^[0-9]+$ ]] && \
    [[ "$family_inventory_bytes" =~ ^[0-9]+$ ]] || return 0
  [ "$app_build" = "$family_build" ] && [ "$app_source" = "$family_source" ] && \
    [ "$app_version" = "$family_version" ] && [ "$app_target" = "$family_target" ] && \
  [ "$app_profile" = "$family_profile" ] && [ "$app_features" = "$family_features" ] && \
    [ "$app_features" = application-family-gui-ft-mux-server-pty-guardian-default-features-v1 ] || {
      warn "FrankenTerm.app identity does not match the installed standalone process family"
      mark_app_skipped app_family_identity_mismatch
      return 0
    }
  app_id="${app_manifest_id#sha256:}"
  [[ "$app_id" =~ ^[0-9a-f]{64}$ ]] || {
    mark_app_skipped invalid_app_manifest_identity
    return 0
  }
  APP_RECEIPT_MANIFEST_ID="$app_manifest_id"

  manifest_store="$dest/.frankenterm-app-manifests"
  mkdir -p "$manifest_store" || return 0
  chmod 0700 "$manifest_store" || return 0
  retained_manifest="$manifest_store/$app_id.json"
  if [ -e "$retained_manifest" ] || [ -L "$retained_manifest" ]; then
    [ -f "$retained_manifest" ] && [ ! -L "$retained_manifest" ] && \
      cmp "$app_manifest" "$retained_manifest" >/dev/null 2>&1 || return 0
  else
    manifest_stage="$manifest_store/.manifest-$app_id.installing"
    ensure_exact_staged_file "$app_manifest" "$manifest_stage" 0444 || return 0
    fsync_installer_tree "$manifest_store" || return 0
    stage_id=$(atomic_path_content_id "$transition_helper" \
      "$manifest_store" "$(basename "$manifest_stage")") || return 0
    txid=$(atomic_transition_txid "app-manifest:$dest:$app_id") || return 0
    atomic_path_transition "$transition_helper" "$manifest_store" \
      "$(basename "$manifest_stage")" "$app_id.json" "$txid" "$stage_id" missing \
      publish-noreplace || return 0
  fi

  target_app="$dest/FrankenTerm.app"
  staged_app="$dest/.FrankenTerm.app.installing-$app_id"
  if [ -d "$target_app" ] && [ ! -L "$target_app" ] && \
      bash "$family_verifier" verify \
        --root "$target_app" --manifest "$retained_manifest" >/dev/null 2>&1; then
    APP_INSTALLED_PATH="$target_app"
    APP_ACTIVATION_STATE="current"
    APP_RECEIPT_CANDIDATE_PATH="$target_app"
    APP_RECEIPT_READINESS="existing_manifest_verified"
    APP_RECEIPT_RESULT="verified"
    APP_RECEIPT_REASON="already_current"
    ok "FrankenTerm.app already matches atomic app generation $app_id"
    return 0
  fi
  APP_RECEIPT_CANDIDATE_PATH="$staged_app"
  if { [ -e "$target_app" ] || [ -L "$target_app" ]; } && \
     { [ ! -d "$target_app" ] || [ -L "$target_app" ]; }; then
    warn "Refusing to replace non-directory or symlink app target at $target_app"
    mark_app_skipped unsafe_live_app_target
    return 0
  fi
  if { [ -e "$staged_app" ] || [ -L "$staged_app" ]; } && \
     { [ ! -d "$staged_app" ] || [ -L "$staged_app" ]; }; then
    warn "Retained app stage is not one resumable directory"
    mark_app_skipped unsafe_retained_app_stage
    return 0
  fi
  require_filesystem_capacity "$dest" \
    "$((app_inventory_bytes + INSTALLER_FREE_SPACE_HEADROOM_BYTES))" \
    "atomic app generation" || {
    warn "Insufficient destination capacity for FrankenTerm.app"
    mark_app_skipped insufficient_generation_capacity
    return 0
  }
  if ! ensure_exact_staged_tree "$extracted_app" "$staged_app"; then
    warn "Retained app stage is not an exact resumable prefix of the requested app generation"
    mark_app_skipped retained_app_stage_conflict
    return 0
  fi
  bash "$family_verifier" verify \
    --root "$staged_app" --manifest "$retained_manifest" >/dev/null || return 0
  fsync_installer_tree "$staged_app" || return 0
  bash "$family_verifier" verify \
    --root "$staged_app" --manifest "$retained_manifest" >/dev/null || return 0
  if command -v codesign >/dev/null 2>&1; then
    codesign --verify --deep --strict "$staged_app" >/dev/null 2>&1 || return 0
  fi

  local readiness_harness="$staged_app/Contents/Resources/e2e-native-events.sh"
  local staged_verifier="$staged_app/Contents/Resources/verify-components.sh"
  [ -f "$readiness_harness" ] && [ -x "$readiness_harness" ] && \
    [ ! -L "$readiness_harness" ] && [ -f "$staged_verifier" ] && \
    [ -x "$staged_verifier" ] && [ ! -L "$staged_verifier" ] || {
      warn "FrankenTerm.app lacks its manifest-bound native readiness authorities"
      mark_app_skipped readiness_authority_missing
      return 0
    }
  info "Running non-activating native readiness proof before app selector switch"
  APP_RECEIPT_READINESS="running"
  if ! FRANKENTERM_ALLOW_GUI_E2E=1 \
      FRANKENTERM_GUI="$staged_app/Contents/MacOS/frankenterm-gui" \
      FRANKENTERM_CLI="$staged_app/Contents/MacOS/ft" \
      FRANKENTERM_MUX_SERVER="$staged_app/Contents/MacOS/frankenterm-mux-server" \
      FRANKENTERM_PTY_GUARDIAN="$staged_app/Contents/MacOS/frankenterm-pty-guardian" \
      FRANKENTERM_CANDIDATE_ROOT="$staged_app" \
      FRANKENTERM_COMPONENT_MANIFEST="$retained_manifest" \
      FRANKENTERM_CANDIDATE_SHA="$app_source" \
      FRANKENTERM_BUILD_PROFILE="$app_profile" \
      FRANKENTERM_ATOMIC_MANIFEST_TOOL="$staged_verifier" \
      /bin/bash "$readiness_harness"; then
    warn "FrankenTerm.app native readiness proof failed; live app authority is unchanged"
    APP_RECEIPT_READINESS="failed"
    mark_app_skipped native_readiness_failed
    return 0
  fi
  APP_RECEIPT_READINESS="passed"

  # Publication is deliberately non-activating until one production lifecycle
  # authority serializes every GUI/CLI/mux/guardian launcher, proves guardian
  # PTY handoff and exact successor readiness, and owns rollback under the same
  # lease. A pathname exchange by the installer alone would recreate the mixed
  # live-family window that immutable generations are intended to eliminate.
  # The source-only test harness may cross this boundary to exercise the exact
  # crash and rollback state machine against private fixtures.
  if [ "${FT_INSTALL_TEST_LIBRARY_ONLY:-0}" != 1 ] || \
     [ "${FT_INSTALL_TEST_ALLOW_APP_SELECTOR:-0}" != 1 ]; then
    APP_INSTALLED_PATH="$staged_app"
    APP_ACTIVATION_STATE="pending"
    APP_RECEIPT_RESULT="verified"
    APP_RECEIPT_REASON="activation_pending_lifecycle_transaction"
    warn "FrankenTerm.app candidate is verified and retained; live app authority is unchanged"
    warn "Activation requires the cross-launcher lifetime and guardian-handoff transaction"
    return 0
  fi

  stage_id=$(atomic_path_content_id "$transition_helper" \
    "$dest" "$(basename "$staged_app")") || return 0
  txid=$(atomic_transition_txid "app-publish:$dest:$app_id") || return 0
  if [ -e "$target_app" ]; then
    target_id=$(atomic_path_content_id "$transition_helper" "$dest" FrankenTerm.app) || return 0
    operation=exchange
  else
    target_id=missing
    operation=publish-noreplace
  fi
  installer_failpoint before-app-selector-switch
  atomic_path_transition "$transition_helper" "$dest" \
    "$(basename "$staged_app")" FrankenTerm.app "$txid" "$stage_id" "$target_id" \
    "$operation" || return 0
  installer_failpoint after-app-selector-switch
  if ! bash "$family_verifier" verify \
      --root "$target_app" --manifest "$retained_manifest" >/dev/null || \
     { command -v codesign >/dev/null 2>&1 && \
       ! codesign --verify --deep --strict "$target_app" >/dev/null 2>&1; }; then
    warn "Post-switch app verification failed; restoring the prior app authority"
    local rollback_txid rollback_stage_id rollback_target_id restored_id failed_app
    if [ "$operation" = exchange ]; then
      rollback_stage_id=$(atomic_path_content_id "$transition_helper" \
        "$dest" "$(basename "$staged_app")") || return 1
      rollback_target_id=$(atomic_path_content_id "$transition_helper" \
        "$dest" FrankenTerm.app) || return 1
      rollback_txid=$(atomic_transition_txid "app-rollback:$dest:$app_id") || return 1
      atomic_path_transition "$transition_helper" "$dest" \
        "$(basename "$staged_app")" FrankenTerm.app "$rollback_txid" \
        "$rollback_stage_id" "$rollback_target_id" exchange || return 1
      restored_id=$(atomic_path_content_id "$transition_helper" \
        "$dest" FrankenTerm.app) || return 1
      [ "$restored_id" = "$target_id" ] || {
        err "App rollback completed without restoring the exact prior authority"
        return 1
      }
    else
      # The successful publish consumed the exact staging name, so moving the
      # failed first generation back to that same name is collision-free under
      # the installer lease and restores both the prior absent authority and
      # the original recoverable candidate location.
      failed_app="$(basename "$staged_app")"
      rollback_stage_id=$(atomic_path_content_id "$transition_helper" \
        "$dest" FrankenTerm.app) || return 1
      rollback_txid=$(atomic_transition_txid "app-first-publish-rollback:$dest:$app_id") || return 1
      atomic_path_transition "$transition_helper" "$dest" \
        FrankenTerm.app "$failed_app" "$rollback_txid" \
        "$rollback_stage_id" missing publish-noreplace || return 1
      if [ -e "$target_app" ] || [ -L "$target_app" ]; then
        err "First-publish rollback did not restore the prior absent app authority"
        return 1
      fi
      restored_id=$(atomic_path_content_id "$transition_helper" \
        "$dest" "$failed_app") || return 1
      [ "$restored_id" = "$rollback_stage_id" ] || {
        err "Failed app quarantine differs from the switched candidate authority"
        return 1
      }
    fi
    warn "Candidate app retained for diagnosis; prior app authority was restored"
    mark_app_skipped post_switch_verification_failed
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

  ok "Installed atomic FrankenTerm.app generation $app_id → $target_app"
  if [ "$operation" = exchange ]; then
    info "Previous FrankenTerm.app preserved at $staged_app"
  fi
  APP_INSTALLED_PATH="$target_app"
  APP_ACTIVATION_STATE="current"
  APP_RECEIPT_CANDIDATE_PATH="$target_app"
  APP_RECEIPT_RESULT="verified"
  APP_RECEIPT_REASON="activated_test_selector"
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
  # Build the CLI, mux server, and PTY guardian from the same source identity.
  # Remote-domain releases are an atomic process family; omitting either
  # long-lived service would recreate the stranding failure that the prebuilt
  # archives prevent.
  local panic_contract_tool="$TMP/src/scripts/check-release-panic-contract.sh"
  if [ ! -f "$panic_contract_tool" ] || \
     ! bash "$panic_contract_tool" --profiles-only; then
    err "Source tree does not satisfy the shipped panic-profile contract."
    exit 1
  fi
  local atomic_tool="$TMP/src/scripts/atomic-component-manifest.sh"
  if [ ! -f "$atomic_tool" ] || [ -L "$atomic_tool" ]; then
    err "Atomic component manifest tool is missing or unsafe: $atomic_tool"
    err "Refusing to build an unverifiable CLI/mux/guardian process family."
    exit 1
  fi

  # Derive the same commit/build identity used by release CI.  The explicit
  # target is important: Cargo configuration can otherwise select a different
  # target after we have already frozen the identity that the binaries claim.
  local source_revision=""
  local workspace_version=""
  local build_target=""
  local build_profile="release-interactive"
  local feature_contract="process-family-ft-mux-server-pty-guardian-default-features-v1"
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
      -p frankenterm-mux-server --bin frankenterm-mux-server \
      -p frankenterm-pty-guardian --bin frankenterm-pty-guardian ); then
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
  local guardian_bin="$TMP/src/target/$build_target/$build_profile/frankenterm-pty-guardian"
  [ -x "$bin" ] || { err "Build did not produce $bin"; exit 1; }
  [ -x "$mux_bin" ] || { err "Build did not produce $mux_bin"; exit 1; }
  [ -x "$guardian_bin" ] || { err "Build did not produce $guardian_bin"; exit 1; }

  # Verify the embedded role/build/target/profile/version markers before any
  # live destination is mutated.  A version probe alone cannot distinguish a
  # sealed atomic family from the ordinary `unsealed` development default.
  local proof_root="$TMP/source-family-proof"
  local proof_manifest="$proof_root/source-family.component-manifest.json"
  if ! mkdir "$proof_root" \
      || ! install -m 0755 "$bin" "$proof_root/ft" \
      || ! install -m 0755 "$mux_bin" "$proof_root/frankenterm-mux-server" \
      || ! install -m 0755 "$guardian_bin" "$proof_root/frankenterm-pty-guardian" \
      || ! install -m 0755 "$atomic_tool" "$proof_root/verify-components.sh"; then
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
      --entry executable:pty-guardian:frankenterm-pty-guardian:frankenterm-pty-guardian \
      --entry verifier:offline-verifier:verify-components.sh \
      --source-match verify-components.sh=scripts/atomic-component-manifest.sh \
      --input font.payload=crates/frankenterm/assets/Pragmasevka_NF.zip.zst \
      || ! bash "$atomic_tool" verify \
      --root "$proof_root" \
      --manifest "$proof_manifest"; then
    err "Source-built ft, mux-server, and PTY guardian do not form one sealed atomic family."
    err "Installed bytes are unchanged."
    exit 1
  fi
  install_process_family \
    "$proof_root/ft" \
    "$proof_root/frankenterm-mux-server" \
    "$proof_root/frankenterm-pty-guardian" \
    "$proof_manifest" \
    "$proof_root/verify-components.sh"
}

# Test subprocesses source the exact production functions so failpoint tests
# execute the installer state machine rather than a structural reimplementation.
# The seam is source-only: executing install.sh with this variable is rejected.
if [ "${FT_INSTALL_TEST_LIBRARY_ONLY:-0}" = 1 ]; then
  if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    err "FT_INSTALL_TEST_LIBRARY_ONLY requires sourcing from an isolated test shell"
    exit 2
  fi
  trap - EXIT
  return 0
fi

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
                  [--activate ID --idle-host-confirmed] [--help]

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
  --no-verify        Skip DSR minisign verification (SHA-256 remains required)
  --offline TARBALL  Install from local tarball; require adjacent .sha256 and
                     .minisig files by default; skip all network calls
  --force            Force reinstall even if same version is installed
  --activate ID      Promote the published candidate generation ID (64 hex,
                     from the install receipt) to the current authority so
                     the stable ft / mux-server / pty-guardian paths resolve
                     to it. Requires --idle-host-confirmed.
  --idle-host-confirmed
                     Attest that no FrankenTerm window, mux server, PTY
                     guardian, or ft watcher is running on this host; the
                     installer also checks the process census before acting
  --artifact-url URL Override artifact URL (e.g. custom mirror)
  --checksum HEX     Inline SHA256 (skips checksum fetch)
  --checksum-url URL Override checksum file URL
  --help, -h         Show this message

Environment overrides:
  VERSION, OWNER, REPO, DEST, APP_DEST, ARTIFACT_URL, CHECKSUM, CHECKSUM_URL,
  MINISIGN_SIGNATURE_URL, APP_MINISIGN_SIGNATURE_URL,
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
    --no-verify) NO_MINISIGN=1; shift ;;
    --offline) require_option_value "$1" "${2:-}"; OFFLINE_TARBALL="$2"; shift 2 ;;
    --force) FORCE_INSTALL=1; shift ;;
    --activate) require_option_value "$1" "${2:-}"; ACTIVATE_GENERATION="$2"; shift 2 ;;
    --idle-host-confirmed) IDLE_HOST_CONFIRMED=1; shift ;;
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
# --activate is a self-contained subcommand: it needs only python3 and the
# already-published candidate under $DEST, never the network.
if [ -n "$ACTIVATE_GENERATION" ]; then
  activate_process_family_generation "$ACTIVATE_GENERATION" || exit 1
  emit_process_family_receipt || exit 1
  exit 0
fi

for required in curl tar python3; do
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

require_release_minisign || exit 1

mkdir -p "$DEST" 2>/dev/null || true
preflight_checks

# ───────────────────────────────────────────────────────────────────────────
# Kernel advisory lock held on one permanent, descriptor-pinned inode.
# ───────────────────────────────────────────────────────────────────────────
LOCK_CONTROL_FIFO="/tmp/ft-install-lock-control-$$"
LOCK_READY_FILE="/tmp/ft-install-lock-ready-$$"
umask 077
mkfifo "$LOCK_CONTROL_FIFO" || { err "Cannot create installer lock control FIFO"; exit 1; }
exec 9<> "$LOCK_CONTROL_FIFO"
# shellcheck disable=SC2094 # fd 9 deliberately holds the FIFO open while the child blocks on fd 3.
python3 - "$LOCK_FILE" "$LOCK_READY_FILE" "$LOCK_CONTROL_FIFO" 3<"$LOCK_CONTROL_FIFO" 9>&- <<'PY' &
import fcntl, os, stat, sys

lock_path, ready_path, fifo_path = sys.argv[1:]
flags = os.O_RDWR | os.O_CREAT | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
lock_fd = os.open(lock_path, flags, 0o600)
try:
    observed = os.fstat(lock_fd)
    if (
        not stat.S_ISREG(observed.st_mode)
        or observed.st_uid != os.geteuid()
        or observed.st_nlink != 1
        or observed.st_mode & 0o077
    ):
        raise SystemExit("unsafe installer lock inode")
    fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    os.fchmod(lock_fd, 0o600)
    os.fsync(lock_fd)
    ready_fd = os.open(
        ready_path,
        os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
        0o600,
    )
    try:
        os.write(ready_fd, f"{os.getpid()}:{observed.st_dev}:{observed.st_ino}\n".encode())
        os.fsync(ready_fd)
    finally:
        os.close(ready_fd)
    os.read(3, 1)
finally:
    os.close(lock_fd)
    for path in (ready_path, fifo_path):
        try:
            os.unlink(path)
        except FileNotFoundError:
            pass
PY
LOCK_HOLDER_PID=$!
for ((_lock_wait=0; _lock_wait<100; _lock_wait++)); do
  if [ -s "$LOCK_READY_FILE" ]; then
    LOCKED=1
    break
  fi
  if ! kill -0 "$LOCK_HOLDER_PID" 2>/dev/null; then
    break
  fi
  sleep 0.05
done
if [ "$LOCKED" -ne 1 ]; then
  err "Another installer owns the kernel lock at $LOCK_FILE, or the lock inode is unsafe"
  exit 1
fi

# Already at target version short-circuit (unless --force or offline). This
# check belongs inside the installer lock: otherwise a concurrent same-semver
# process-family publication can be observed between its two canonical moves.
# check_installed_version also requires one exact sealed build identity across
# all three roles; matching --version output alone is not sufficient.
if [ "$FORCE_INSTALL" -eq 0 ] && [ -z "$OFFLINE_TARBALL" ] && [ -n "$VERSION" ] \
    && check_installed_version "$VERSION"; then
  ok "ft + frankenterm-mux-server + frankenterm-pty-guardian $VERSION are already installed at $DEST"
  info "Use --force to reinstall"
  # Still honour the font / GUI-app side installs even when the CLI is current,
  # so a re-run can add the .app to an existing CLI-only install. Decide once
  # (should_install_app may warn under --with-app) to avoid a double call.
  app_wanted=0
  if should_install_app; then app_wanted=1; fi
  if [ "$WITH_FONT" -eq 1 ] || [ "$app_wanted" -eq 1 ]; then
    TMP=$(mktemp -d)
    if [ "$WITH_FONT" -eq 1 ]; then install_pragmasevka; fi
    if [ "$app_wanted" -eq 1 ] && ! install_macos_app; then
      mark_app_skipped app_install_transaction_failed
      finalize_app_receipt_state
      emit_app_receipt || true
      exit 1
    fi
  fi
  finalize_app_receipt_state
  emit_app_receipt || exit 1
  exit 0
fi

TMP=$(mktemp -d)

# Refuse before the first archive byte is transferred unless both the private
# temporary workspace (compressed bytes plus bounded extraction) and the final
# destination (bounded installed generation) have enough capacity. Exact
# authenticated inventory sizes are checked again before publication.
if [ "$FROM_SOURCE" -eq 0 ]; then
  require_transfer_capacity "$TMP" "$DEST" \
    "$MAX_PROCESS_ARCHIVE_BYTES" "$MAX_PROCESS_EXPANDED_BYTES" \
    "standalone process-family" || exit 1
fi

# ───────────────────────────────────────────────────────────────────────────
# Download / source build / offline-tarball selection
# ───────────────────────────────────────────────────────────────────────────
if [ -n "$OFFLINE_TARBALL" ]; then
  info "Using offline tarball: $OFFLINE_TARBALL"
  ensure_exact_staged_file "$OFFLINE_TARBALL" "$TMP/$TAR" 0400 || {
    err "Offline archive is not one stable bounded no-follow regular file"
    exit 1
  }
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
        download_https_bounded "$URL" "$TMP/$TAR" \
          "$MAX_PROCESS_ARCHIVE_BYTES" 300 1; then
      warn "Artifact download failed; falling back to build-from-source"
      FROM_SOURCE=1
    fi
  fi
fi

if [ "$FROM_SOURCE" -eq 1 ]; then
  build_from_source
else
  # The archive-provided verifier is executable code, so one externally
  # supplied SHA-256 authority is mandatory before extraction or execution.
  # --no-verify disables only the DSR minisign layer.
  verify_archive_checksum_authority "$TMP/$TAR" "$TAR" || {
    err "Installation aborted before archive extraction"
    exit 1
  }
  if [ "$NO_MINISIGN" -eq 0 ] && [ -n "$OFFLINE_TARBALL" ]; then
    offline_minisign="${OFFLINE_TARBALL}.minisig"
    [ -f "$offline_minisign" ] && [ ! -L "$offline_minisign" ] || {
      err "Offline DSR minisign signature not found: $offline_minisign"
      err "Use --no-verify only for an explicitly trusted local test artifact."
      exit 1
    }
    verify_minisign_signature "$TMP/$TAR" "" \
      "$VERIFIED_ARCHIVE_IDENTITY" "" "$offline_minisign" || {
      err "Offline signature verification failed"
      exit 1
    }
  elif [ "$NO_MINISIGN" -eq 0 ] && [ -n "$URL" ]; then
    verify_minisign_signature "$TMP/$TAR" "$URL" \
      "$VERIFIED_ARCHIVE_IDENTITY" "$MINISIGN_SIGNATURE_URL" || {
      err "Signature verification failed"
      exit 1
    }
  elif [ "$NO_MINISIGN" -eq 1 ]; then
    warn "DSR minisign verification skipped (--no-verify); SHA-256 was still verified"
  fi

  # Extract into an otherwise-empty private package root with a two-pass
  # streaming inventory. No archive member can escape or allocate an unbounded
  # retained metadata list before the exact five-file namespace is accepted.
  PACKAGE_ROOT="$TMP/package"
  if ! mkdir -m 0700 "$PACKAGE_ROOT"; then
    err "Failed to create package verification directory"
    exit 1
  fi
  info "Extracting $TAR"
  if ! extract_authenticated_archive "$TMP/$TAR" "$PACKAGE_ROOT" process-family \
      "${ASSET%.tar.xz}.component-manifest.json" "$VERIFIED_ARCHIVE_IDENTITY"; then
    err "Failed to extract $TAR — archive may be corrupt or truncated"
    err "If the download was interrupted, retry; otherwise file an issue at:"
    err "  https://github.com/${OWNER}/${REPO}/issues"
    exit 1
  fi

  # A checksum proves archive bytes, but it cannot prove that the CLI, mux
  # server, and PTY guardian came from one source/build identity. Keep this
  # atomic process-family verification mandatory after the externally rooted
  # archive proof.
  COMPONENT_VERIFIER="$PACKAGE_ROOT/verify-components.sh"
  COMPONENT_MANIFEST="$PACKAGE_ROOT/${ASSET%.tar.xz}.component-manifest.json"
  [ -f "$COMPONENT_VERIFIER" ] || {
    err "Atomic component verifier not found in tarball"
    err "Refusing an unverifiable CLI/mux/guardian process-family install"
    exit 1
  }
  [ -f "$COMPONENT_MANIFEST" ] || {
    err "Atomic component manifest not found in tarball: $(basename "$COMPONENT_MANIFEST")"
    err "Refusing an unverifiable CLI/mux/guardian process-family install"
    exit 1
  }
  info "Verifying atomic CLI/mux-server/PTY-guardian build identity"
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
  GUARDIAN_BIN="$PACKAGE_ROOT/frankenterm-pty-guardian"
  if [ ! -x "$GUARDIAN_BIN" ]; then
    GUARDIAN_BIN=$(find "$PACKAGE_ROOT" -maxdepth 3 -type f -name "frankenterm-pty-guardian" -perm -111 2>/dev/null | head -n 1)
  fi
  [ -x "$GUARDIAN_BIN" ] || {
    err "frankenterm-pty-guardian binary not found in tarball"
    err "Refusing an incomplete install that cannot preserve PTY ownership across mux handoff"
    exit 1
  }
  install_process_family "$BIN" "$MUX_BIN" "$GUARDIAN_BIN" \
    "$COMPONENT_MANIFEST" "$COMPONENT_VERIFIER"
fi

# ───────────────────────────────────────────────────────────────────────────
# Post-install
# ───────────────────────────────────────────────────────────────────────────
if [ "$PROCESS_FAMILY_ACTIVE_AUTHORITY" != none ]; then
  maybe_add_path
fi

if [ "$WITH_FONT" -eq 1 ]; then
  install_pragmasevka
fi

if should_install_app; then
  if ! install_macos_app; then
    mark_app_skipped app_install_transaction_failed
    finalize_app_receipt_state
    emit_app_receipt || true
    exit 1
  fi
fi

if [ "$VERIFY" -eq 1 ]; then
  if [ -n "$PENDING_PROCESS_FAMILY_GENERATION" ]; then
    warn "Skipping candidate self-test because the candidate is not activated"
  else
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
fi

# The candidate manifest came from the externally authenticated archive (or
# the caller-owned source build) and is the only version authority needed for
# this summary. Never execute an older destination pathname merely to decorate
# success output.
[ -n "$PUBLISHED_PROCESS_FAMILY_VERSION" ] || {
  err "Authenticated process-family version receipt is absent"
  exit 1
}
RESOLVED_VERSION="ft $PUBLISHED_PROCESS_FAMILY_VERSION"

# ───────────────────────────────────────────────────────────────────────────
# Final summary
# ───────────────────────────────────────────────────────────────────────────
if [ "$QUIET" -eq 0 ]; then
  summary_lines=()
  if [ -n "$PENDING_PROCESS_FAMILY_GENERATION" ]; then
    summary_lines+=("\033[1;32mFrankenTerm candidate published\033[0m")
  else
    summary_lines+=("\033[1;32mFrankenTerm installed\033[0m")
  fi
  summary_lines+=("")
  if [ -n "$PENDING_PROCESS_FAMILY_GENERATION" ]; then
    summary_lines+=("Candidate CLI:      $PUBLISHED_PROCESS_FAMILY_ROOT/ft")
    summary_lines+=("Candidate mux:      $PUBLISHED_PROCESS_FAMILY_ROOT/frankenterm-mux-server")
    summary_lines+=("Candidate guardian: $PUBLISHED_PROCESS_FAMILY_ROOT/frankenterm-pty-guardian")
    summary_lines+=("Candidate version:  $RESOLVED_VERSION")
    summary_lines+=("Candidate ID:       $PENDING_PROCESS_FAMILY_GENERATION")
    summary_lines+=("Active authority:   $PROCESS_FAMILY_ACTIVE_AUTHORITY")
    if [ -n "$PROCESS_FAMILY_ACTIVE_ROOT" ]; then
      summary_lines+=("Active root:        $PROCESS_FAMILY_ACTIVE_ROOT")
    else
      summary_lines+=("Active root:        none")
    fi
    summary_lines+=("Pending reason:     $PROCESS_FAMILY_PENDING_REASON")
    summary_lines+=("Activation: pending; existing selector and live mux unchanged")
  else
    summary_lines+=("Binary:   $DEST/ft")
    summary_lines+=("Mux:      $DEST/frankenterm-mux-server")
    summary_lines+=("Guardian: $DEST/frankenterm-pty-guardian")
    summary_lines+=("Version:  $RESOLVED_VERSION")
  fi
  if [ -n "${TARGET:-}" ]; then
    summary_lines+=("Platform: ${OS}/${ARCH} ($TARGET)")
  else
    summary_lines+=("Platform: ${OS}/${ARCH}")
  fi
  if [ -n "$FONT_INSTALLED_PATH" ]; then
    summary_lines+=("Font:     Pragmasevka NF installed at $FONT_INSTALLED_PATH")
  fi
  if [ -n "$APP_INSTALLED_PATH" ]; then
    if [ "$APP_ACTIVATION_STATE" = pending ]; then
      summary_lines+=("GUI candidate:       $APP_INSTALLED_PATH")
      summary_lines+=("GUI activation:      pending; live app authority unchanged")
    else
      summary_lines+=("GUI app:  $APP_INSTALLED_PATH")
    fi
  fi
  summary_lines+=("")
  if [ -n "$PENDING_PROCESS_FAMILY_GENERATION" ]; then
    summary_lines+=("Candidate publication is complete; the installer never activates it automatically.")
    summary_lines+=("To activate on an idle host (no FrankenTerm window, mux server, guardian, or watcher running):")
    summary_lines+=("  install.sh --dest $DEST --activate $PENDING_PROCESS_FAMILY_GENERATION --idle-host-confirmed")
  else
    summary_lines+=("Quick start:")
    summary_lines+=("  ft --help               Show all subcommands")
    summary_lines+=("  ft version --full       Build metadata (commit / rustc / features)")
    summary_lines+=("  ft doctor --json        Diagnostic snapshot")
    summary_lines+=("  ft session list         Inspect running sessions")
  fi
  summary_lines+=("")
  if [ -n "$PENDING_PROCESS_FAMILY_GENERATION" ]; then
    summary_lines+=("Candidate cleanup (active authority is not changed):")
    summary_lines+=("  rm -r $PUBLISHED_PROCESS_FAMILY_ROOT")
  else
    summary_lines+=("Uninstall:")
    summary_lines+=("  rm $DEST/ft")
    summary_lines+=("  rm $DEST/frankenterm-mux-server")
    summary_lines+=("  rm $DEST/frankenterm-pty-guardian")
  fi
  if [ -n "$FONT_INSTALLED_PATH" ]; then
    # Select the right font path based on the platform we installed for —
    # don't concatenate Linux + macOS paths together.
    case "$OS" in
      linux|darwin) summary_lines+=("  rm -r $FONT_INSTALLED_PATH") ;;
    esac
  fi
  if [ -n "$APP_INSTALLED_PATH" ]; then
    summary_lines+=("  rm -r $APP_INSTALLED_PATH")
  fi
  summary_lines+=("")
  summary_lines+=("Docs:     https://github.com/${OWNER}/${REPO}")
  echo
  draw_box "0;32" ${summary_lines[@]+"${summary_lines[@]}"}
  echo
fi

# This exact one-line receipt is intentionally emitted even under --quiet.
# Automation must branch on `activation`; exit zero alone never means that a
# selector or live mux was changed.
emit_process_family_receipt || exit 1
finalize_app_receipt_state
emit_app_receipt || exit 1
