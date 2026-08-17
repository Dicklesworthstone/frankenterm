#!/usr/bin/env bash
set -euo pipefail

# Assemble FrankenTerm's exact Node + Playwright + Chromium capability without
# invoking npm, npx, Node, Playwright, or a browser. Every network payload is
# pinned by docs/release/browser-runtime-lock.v1.json and verified before use.
# Output and cache paths are append-only: existing files are never overwritten
# or removed, and interrupted .partial files are never admitted as authority.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LOCK_FILE="$PROJECT_ROOT/docs/release/browser-runtime-lock.v1.json"
MANIFEST_TOOL="$PROJECT_ROOT/scripts/atomic-component-manifest.sh"
TARGET=""
OUTPUT=""
MANIFEST=""
CACHE=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --target) TARGET="$2"; shift 2 ;;
        --output) OUTPUT="$2"; shift 2 ;;
        --manifest) MANIFEST="$2"; shift 2 ;;
        --cache) CACHE="$2"; shift 2 ;;
        -h|--help)
            echo "Usage: $0 --target TRIPLE --output DIR --manifest FILE --cache DIR"
            echo "Assembles and verifies the pinned browser component without launching it."
            exit 0
            ;;
        *) echo "Unknown option: $1" >&2; exit 2 ;;
    esac
done

if [[ -z "$TARGET" || -z "$OUTPUT" || -z "$MANIFEST" || -z "$CACHE" ]]; then
    echo "Error: --target, --output, --manifest, and --cache are required" >&2
    exit 2
fi
case "$TARGET" in
    aarch64-apple-darwin|x86_64-apple-darwin) ;;
    *) echo "Error: unsupported browser runtime target '$TARGET'" >&2; exit 2 ;;
esac
if [[ -e "$OUTPUT" || -e "$MANIFEST" ]]; then
    echo "Error: output and manifest paths must both be fresh" >&2
    exit 2
fi
for command in curl git ln python3 shasum; do
    if ! command -v "$command" >/dev/null 2>&1; then
        echo "Error: required command '$command' is unavailable" >&2
        exit 2
    fi
done
if [[ ! -f "$LOCK_FILE" || ! -x "$MANIFEST_TOOL" ]]; then
    echo "Error: browser lock or atomic manifest tool is unavailable" >&2
    exit 2
fi
SOURCE_REVISION=$(git -C "$PROJECT_ROOT" rev-parse HEAD)
if [[ ! "$SOURCE_REVISION" =~ ^[0-9a-f]{40}$ ]]; then
    echo "Error: cannot resolve a full source revision for browser runtime assembly" >&2
    exit 2
fi
UNTRACKED_SOURCE=$(git -C "$PROJECT_ROOT" ls-files --others --exclude-standard)
if ! git -C "$PROJECT_ROOT" diff --quiet -- \
    || ! git -C "$PROJECT_ROOT" diff --cached --quiet -- \
    || [[ -n "$UNTRACKED_SOURCE" ]]; then
    echo "Error: source changes are present; refusing commit-bound browser assembly" >&2
    echo "Commit the intended source snapshot, then assemble the browser runtime." >&2
    exit 2
fi

LOCK_VALUES=()
while IFS= read -r lock_value_record; do
    LOCK_VALUES+=("$lock_value_record")
done < <(python3 - "$LOCK_FILE" "$TARGET" <<'PY'
import json
import re
import sys
from pathlib import PurePosixPath
from urllib.parse import urlsplit

path, target = sys.argv[1:]
with open(path, "rb") as handle:
    value = json.load(handle)
if set(value) != {
    "$schema", "schema_version", "installed_byte_budget", "required_free_bytes",
    "node", "playwright", "chromium", "protocol_version", "targets",
}:
    raise SystemExit("browser lock has an unexpected top-level field set")
if value["$schema"] != "https://frankenterm.dev/schemas/ft-browser-runtime-lock/v1.json":
    raise SystemExit("browser lock schema URI mismatch")
if value["schema_version"] != "ft.browser_runtime_lock.v1":
    raise SystemExit("browser lock schema version mismatch")
if set(value["node"]) != {"version"}:
    raise SystemExit("browser lock node field set mismatch")
if set(value["playwright"]) != {
    "archive_sha256", "archive_url", "core_archive_sha256",
    "core_archive_url", "version",
}:
    raise SystemExit("browser lock Playwright field set mismatch")
if set(value["chromium"]) != {"browser_version", "revision"}:
    raise SystemExit("browser lock Chromium field set mismatch")
if set(value["targets"]) != {"aarch64-apple-darwin", "x86_64-apple-darwin"}:
    raise SystemExit("browser lock target field set mismatch")
target_value = value["targets"].get(target)
if not isinstance(target_value, dict):
    raise SystemExit("browser lock has no requested target")
if set(target_value) != {
    "chromium_archive_root", "chromium_archive_sha256", "chromium_archive_url",
    "chromium_executable", "node_archive_root", "node_archive_sha256",
    "node_archive_url",
}:
    raise SystemExit("browser lock selected-target field set mismatch")
fields = {
    "installed_byte_budget": value["installed_byte_budget"],
    "required_free_bytes": value["required_free_bytes"],
    "node_version": value["node"]["version"],
    "playwright_version": value["playwright"]["version"],
    "playwright_url": value["playwright"]["archive_url"],
    "playwright_sha256": value["playwright"]["archive_sha256"],
    "playwright_core_url": value["playwright"]["core_archive_url"],
    "playwright_core_sha256": value["playwright"]["core_archive_sha256"],
    "chromium_version": value["chromium"]["browser_version"],
    "chromium_revision": value["chromium"]["revision"],
    "protocol_version": value["protocol_version"],
    **target_value,
}
sha256 = re.compile(r"^[0-9a-f]{64}$")
token = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._+-]*$")
for key, item in fields.items():
    if key in {"installed_byte_budget", "required_free_bytes"}:
        if type(item) is not int or item <= 0:
            raise SystemExit(f"invalid positive byte count: {key}")
    elif key.endswith("_sha256"):
        if not isinstance(item, str) or not sha256.fullmatch(item):
            raise SystemExit(f"invalid SHA-256: {key}")
    elif key.endswith("_url"):
        parsed = urlsplit(item) if isinstance(item, str) else None
        if (
            parsed is None
            or parsed.scheme != "https"
            or not parsed.hostname
            or parsed.username is not None
            or parsed.password is not None
            or parsed.fragment
            or any(ord(char) < 0x20 for char in item)
        ):
            raise SystemExit(f"invalid HTTPS URL: {key}")
    elif key.endswith("_version") or key in {"chromium_revision", "protocol_version"}:
        if not isinstance(item, str) or not token.fullmatch(item):
            raise SystemExit(f"invalid token: {key}")
    else:
        if not isinstance(item, str) or not item or "\\" in item or "\x00" in item:
            raise SystemExit(f"invalid path: {key}")
        path_item = PurePosixPath(item)
        if path_item.is_absolute() or any(part in {"", ".", ".."} for part in path_item.parts):
            raise SystemExit(f"invalid relative path: {key}")
        if key.endswith("_root") and len(path_item.parts) != 1:
            raise SystemExit(f"archive root must be one path component: {key}")
for key in sorted(fields):
    print(f"{key}={fields[key]}")
PY
)

lock_value() {
    local wanted="$1"
    local record
    for record in "${LOCK_VALUES[@]}"; do
        if [[ "$record" == "$wanted="* ]]; then
            printf '%s\n' "${record#*=}"
            return 0
        fi
    done
    return 1
}

INSTALLED_BYTE_BUDGET=$(lock_value installed_byte_budget)
REQUIRED_FREE_BYTES=$(lock_value required_free_bytes)
NODE_VERSION=$(lock_value node_version)
NODE_URL=$(lock_value node_archive_url)
NODE_SHA256=$(lock_value node_archive_sha256)
NODE_ARCHIVE_ROOT=$(lock_value node_archive_root)
PLAYWRIGHT_VERSION=$(lock_value playwright_version)
PLAYWRIGHT_URL=$(lock_value playwright_url)
PLAYWRIGHT_SHA256=$(lock_value playwright_sha256)
PLAYWRIGHT_CORE_URL=$(lock_value playwright_core_url)
PLAYWRIGHT_CORE_SHA256=$(lock_value playwright_core_sha256)
CHROMIUM_VERSION=$(lock_value chromium_version)
CHROMIUM_REVISION=$(lock_value chromium_revision)
CHROMIUM_URL=$(lock_value chromium_archive_url)
CHROMIUM_SHA256=$(lock_value chromium_archive_sha256)
CHROMIUM_ARCHIVE_ROOT=$(lock_value chromium_archive_root)
CHROMIUM_EXECUTABLE_REL=$(lock_value chromium_executable)
PROTOCOL_VERSION=$(lock_value protocol_version)

mkdir -p "$CACHE" "$(dirname "$OUTPUT")" "$(dirname "$MANIFEST")"
AVAILABLE_KIB=$(df -Pk "$(dirname "$OUTPUT")" | awk 'NR == 2 { print $4 }')
if [[ ! "$AVAILABLE_KIB" =~ ^[0-9]+$ ]]; then
    echo "Error: could not determine available output filesystem capacity" >&2
    exit 2
fi
AVAILABLE_BYTES=$((AVAILABLE_KIB * 1024))
if (( AVAILABLE_BYTES < REQUIRED_FREE_BYTES )); then
    echo "Error: browser runtime assembly requires at least $REQUIRED_FREE_BYTES free bytes" >&2
    exit 2
fi

verify_sha256() {
    local path="$1"
    local expected="$2"
    local actual
    actual=$(shasum -a 256 "$path" | awk '{print $1}')
    [[ "$actual" == "$expected" ]]
}

fetch_locked() {
    local url="$1"
    local expected="$2"
    local suffix="$3"
    local destination="$CACHE/${expected}.${suffix}"
    if [[ -f "$destination" ]]; then
        if ! verify_sha256 "$destination" "$expected"; then
            echo "Error: cached browser payload digest mismatch: $destination" >&2
            return 1
        fi
        printf '%s\n' "$destination"
        return 0
    fi
    local partial="$CACHE/${expected}.partial.$$.${suffix}"
    if [[ -e "$partial" ]]; then
        echo "Error: fresh partial download path is already occupied: $partial" >&2
        return 1
    fi
    curl --fail --location --silent --show-error \
        --proto '=https' --tlsv1.2 --retry 3 \
        --output "$partial" "$url"
    if ! verify_sha256 "$partial" "$expected"; then
        echo "Error: downloaded browser payload digest mismatch: $url" >&2
        return 1
    fi
    if ! ln "$partial" "$destination"; then
        echo "Error: browser payload cache destination appeared concurrently" >&2
        return 1
    fi
    printf '%s\n' "$destination"
}

NODE_ARCHIVE=$(fetch_locked "$NODE_URL" "$NODE_SHA256" tar.xz)
PLAYWRIGHT_ARCHIVE=$(fetch_locked "$PLAYWRIGHT_URL" "$PLAYWRIGHT_SHA256" tgz)
PLAYWRIGHT_CORE_ARCHIVE=$(fetch_locked "$PLAYWRIGHT_CORE_URL" "$PLAYWRIGHT_CORE_SHA256" tgz)
CHROMIUM_ARCHIVE=$(fetch_locked "$CHROMIUM_URL" "$CHROMIUM_SHA256" zip)

RUNTIME="$OUTPUT/runtime"
mkdir -p \
    "$RUNTIME/bin" \
    "$RUNTIME/node_modules/playwright" \
    "$RUNTIME/node_modules/playwright-core" \
    "$RUNTIME/browsers/chromium-$CHROMIUM_REVISION" \
    "$RUNTIME/licenses"

# Extract only the two exact regular-file members needed from the pinned Node
# archive. No archive-supplied path is ever used as an output path.
python3 - \
    "$NODE_ARCHIVE" \
    "$NODE_SHA256" \
    "$NODE_ARCHIVE_ROOT" \
    "$RUNTIME/bin/node" \
    "$RUNTIME/licenses/node.txt" \
    "$INSTALLED_BYTE_BUDGET" <<'PY'
import hashlib
import shutil
import sys
import tarfile
from pathlib import Path

archive_raw, expected_sha256, archive_root, node_raw, license_raw, byte_budget_raw = sys.argv[1:]
archive_path = Path(archive_raw)
outputs = {
    f"{archive_root}/bin/node": Path(node_raw),
    f"{archive_root}/LICENSE": Path(license_raw),
}
byte_budget = int(byte_budget_raw)

with archive_path.open("rb") as archive_handle:
    actual_sha256 = hashlib.file_digest(archive_handle, "sha256").hexdigest()
    if actual_sha256 != expected_sha256:
        raise SystemExit("Node archive digest changed before extraction")
    archive_handle.seek(0)
    with tarfile.open(fileobj=archive_handle, mode="r:xz") as source:
        members = source.getmembers()
        if not members or len(members) > 100_000:
            raise SystemExit("Node archive has an invalid entry count")
        if sum(member.size for member in members) > byte_budget:
            raise SystemExit("Node archive exceeds the installed-byte budget")
        selected = {}
        for member in members:
            if member.name not in outputs:
                continue
            if member.name in selected or not member.isfile():
                raise SystemExit("Node archive required member is repeated or non-regular")
            selected[member.name] = member
        if set(selected) != set(outputs):
            raise SystemExit("Node archive is missing a required member")
        for member_name, destination in outputs.items():
            member = selected[member_name]
            reader = source.extractfile(member)
            if reader is None:
                raise SystemExit("Node archive required member has no payload")
            with reader, destination.open("xb") as writer:
                shutil.copyfileobj(reader, writer, length=1024 * 1024)
    archive_handle.seek(0)
    final_sha256 = hashlib.file_digest(archive_handle, "sha256").hexdigest()
    if final_sha256 != expected_sha256:
        raise SystemExit("Node archive digest changed during extraction")
PY
chmod 0755 "$RUNTIME/bin/node"

# npm package archives are not delegated to tar. Validate the complete member
# table before creating any package file, then revalidate while extracting only
# regular files and directories beneath the exact `package/` root. The input
# digest is checked both before and after extraction so an in-place cache
# mutation cannot be admitted into the component manifest.
extract_npm_archive() {
    local archive="$1"
    local expected_sha256="$2"
    local destination="$3"
    python3 - "$archive" "$expected_sha256" "$destination" "$INSTALLED_BYTE_BUDGET" <<'PY'
import hashlib
import os
import shutil
import sys
import tarfile
from pathlib import Path, PurePosixPath

archive_raw, expected_sha256, destination_raw, byte_budget_raw = sys.argv[1:]
archive_path = Path(archive_raw)
destination = Path(destination_raw)
byte_budget = int(byte_budget_raw)

def validated_members(source):
    members = source.getmembers()
    if not members or len(members) > 100_000:
        raise SystemExit("npm archive has an invalid entry count")
    seen = set()
    total = 0
    for member in members:
        if (
            "\\" in member.name
            or "\x00" in member.name
            or len(member.name.encode("utf-8")) > 4096
        ):
            raise SystemExit("npm archive contains an invalid path encoding")
        relative = PurePosixPath(member.name)
        if (
            relative.is_absolute()
            or len(relative.parts) < 2
            or relative.parts[0] != "package"
            or any(part in {"", ".", ".."} for part in relative.parts)
        ):
            raise SystemExit("npm archive contains an unsafe path")
        output_relative = PurePosixPath(*relative.parts[1:])
        if output_relative in seen:
            raise SystemExit("npm archive repeats an output path")
        seen.add(output_relative)
        if not member.isfile() and not member.isdir():
            raise SystemExit("npm archive contains a link or special entry")
        if member.size < 0:
            raise SystemExit("npm archive contains a negative member size")
        total += member.size
        if total > byte_budget:
            raise SystemExit("npm archive exceeds the installed-byte budget")
    return members

with archive_path.open("rb") as archive_handle:
    actual_sha256 = hashlib.file_digest(archive_handle, "sha256").hexdigest()
    if actual_sha256 != expected_sha256:
        raise SystemExit("npm archive digest changed before extraction")
    archive_handle.seek(0)
    with tarfile.open(fileobj=archive_handle, mode="r:gz") as source:
        validated_members(source)

    archive_handle.seek(0)
    with tarfile.open(fileobj=archive_handle, mode="r:gz") as source:
        for member in validated_members(source):
            relative = PurePosixPath(member.name)
            output_relative = PurePosixPath(*relative.parts[1:])
            output_path = destination.joinpath(*output_relative.parts)
            if member.isdir():
                output_path.mkdir(parents=True, exist_ok=True)
                continue
            output_path.parent.mkdir(parents=True, exist_ok=True)
            if output_path.exists():
                raise SystemExit("npm archive output path is already occupied")
            reader = source.extractfile(member)
            if reader is None:
                raise SystemExit("npm archive regular member has no payload")
            with reader, output_path.open("xb") as writer:
                shutil.copyfileobj(reader, writer, length=1024 * 1024)
            mode = member.mode & 0o777
            os.chmod(output_path, mode if mode else 0o644)

    archive_handle.seek(0)
    final_sha256 = hashlib.file_digest(archive_handle, "sha256").hexdigest()
    if final_sha256 != expected_sha256:
        raise SystemExit("npm archive digest changed during extraction")
PY
}

extract_npm_archive \
    "$PLAYWRIGHT_ARCHIVE" \
    "$PLAYWRIGHT_SHA256" \
    "$RUNTIME/node_modules/playwright"
extract_npm_archive \
    "$PLAYWRIGHT_CORE_ARCHIVE" \
    "$PLAYWRIGHT_CORE_SHA256" \
    "$RUNTIME/node_modules/playwright-core"
cp "$RUNTIME/node_modules/playwright/LICENSE" "$RUNTIME/licenses/playwright.txt"

# Zip members are extracted without trusting absolute/traversal paths. macOS
# framework symlinks are retained only after their targets are proved to stay
# within the extracted runtime root. A hashed sidecar binds their exact paths
# and targets for the browser-runtime-only verifier allowance.
python3 - "$CHROMIUM_ARCHIVE" "$CHROMIUM_SHA256" "$RUNTIME/browsers/chromium-$CHROMIUM_REVISION" "$CHROMIUM_ARCHIVE_ROOT" "$INSTALLED_BYTE_BUDGET" <<'PY'
import hashlib
import os
import shutil
import stat
import sys
import zipfile
from pathlib import Path, PurePosixPath

archive, expected_sha256, output_raw, expected_root, byte_budget_raw = sys.argv[1:]
output = Path(output_raw)
byte_budget = int(byte_budget_raw)
symlinks = []
seen = set()
total = 0
with open(archive, "rb") as archive_handle:
    if hashlib.file_digest(archive_handle, "sha256").hexdigest() != expected_sha256:
        raise SystemExit("Chromium archive digest changed before extraction")
with zipfile.ZipFile(archive) as source:
    infos = source.infolist()
    if len(infos) > 100_000:
        raise SystemExit("Chromium archive exceeds the entry bound")
    for info in infos:
        if (
            "\\" in info.filename
            or "\x00" in info.filename
            or len(info.filename.encode("utf-8")) > 4096
        ):
            raise SystemExit("Chromium archive contains an invalid path encoding")
        relative = PurePosixPath(info.filename)
        if relative.is_absolute() or not relative.parts or relative.parts[0] != expected_root:
            raise SystemExit("Chromium archive has an unexpected root")
        if any(part in {"", ".", ".."} for part in relative.parts):
            raise SystemExit("Chromium archive contains an unsafe path")
        if relative in seen:
            raise SystemExit("Chromium archive repeats an output path")
        seen.add(relative)
        total += info.file_size
        if total > byte_budget:
            raise SystemExit("Chromium archive exceeds the installed-byte budget")
        mode = info.external_attr >> 16
        destination = output.joinpath(*relative.parts)
        if stat.S_ISLNK(mode):
            target = source.read(info).decode("utf-8")
            if (
                not target
                or "\x00" in target
                or "\\" in target
                or len(target.encode("utf-8")) > 4096
                or PurePosixPath(target).is_absolute()
            ):
                raise SystemExit("Chromium archive contains an invalid symlink target")
            symlinks.append((destination, target))
            if len(symlinks) > 256:
                raise SystemExit("Chromium archive exceeds the symlink bound")
            continue
        if info.is_dir():
            destination.mkdir(parents=True, exist_ok=True)
            continue
        if mode and not stat.S_ISREG(mode):
            raise SystemExit("Chromium archive contains a special filesystem entry")
        destination.parent.mkdir(parents=True, exist_ok=True)
        if destination.exists():
            raise SystemExit("Chromium archive repeats an output path")
        with source.open(info) as reader, destination.open("xb") as writer:
            shutil.copyfileobj(reader, writer, length=1024 * 1024)
        os.chmod(destination, mode & 0o777 if mode & 0o777 else 0o644)

def resolve_target(link_path, target):
    candidate = (link_path.parent / target).resolve(strict=False)
    root = output.resolve(strict=True)
    try:
        candidate.relative_to(root)
    except ValueError as exc:
        raise SystemExit("Chromium archive symlink escapes its root") from exc
    return candidate

pending = list(symlinks)
for _ in range(len(pending) + 1):
    if not pending:
        break
    deferred = []
    progressed = False
    for link_path, target in pending:
        source_path = resolve_target(link_path, target)
        if not source_path.exists():
            deferred.append((link_path, target))
            continue
        if link_path.exists():
            raise SystemExit("Chromium materialized-link path already exists")
        if not source_path.is_dir() and not source_path.is_file():
            raise SystemExit("Chromium symlink target is not regular")
        link_path.parent.mkdir(parents=True, exist_ok=True)
        os.symlink(target, link_path)
        progressed = True
    if not progressed and deferred:
        raise SystemExit("Chromium archive contains an unresolved symlink cycle")
    pending = deferred
if pending:
    raise SystemExit("Chromium archive symlink materialization was incomplete")
with open(archive, "rb") as archive_handle:
    if hashlib.file_digest(archive_handle, "sha256").hexdigest() != expected_sha256:
        raise SystemExit("Chromium archive digest changed during extraction")
PY

SYMLINK_MANIFEST_REL="runtime/browser-symlinks.v1.json"
python3 - "$RUNTIME" "$OUTPUT/$SYMLINK_MANIFEST_REL" <<'PY'
import json
import os
import sys
from pathlib import Path

root = Path(sys.argv[1]).resolve(strict=True)
output = Path(sys.argv[2])
links = []
pending = [root]
while pending:
    directory = pending.pop()
    with os.scandir(directory) as entries:
        for entry in entries:
            path = Path(entry.path)
            relative = path.relative_to(root).as_posix()
            if entry.is_symlink():
                links.append({"path": relative, "target": os.readlink(path)})
            elif entry.is_dir(follow_symlinks=False):
                pending.append(path)
links.sort(key=lambda item: item["path"])
payload = {
    "schema_version": "ft.browser_runtime_symlinks.v1",
    "links": links,
}
with output.open("x", encoding="utf-8") as handle:
    json.dump(payload, handle, ensure_ascii=False, indent=2, sort_keys=True)
    handle.write("\n")
PY

: > "$RUNTIME/browsers/chromium-$CHROMIUM_REVISION/INSTALLATION_COMPLETE"
printf '%s\n' \
    "Chrome for Testing $CHROMIUM_VERSION" \
    "Upstream artifact: $CHROMIUM_URL" \
    "Pinned SHA-256: $CHROMIUM_SHA256" \
    "Purpose: browser automation against trustworthy content only." \
    "Project information and terms: https://developer.chrome.com/blog/chrome-for-testing" \
    "Open-source notices are available from the browser's chrome://credits surface." \
    > "$RUNTIME/licenses/chrome-for-testing-NOTICE.txt"

CHROMIUM_PATH="runtime/browsers/chromium-$CHROMIUM_REVISION/$CHROMIUM_EXECUTABLE_REL"
if [[ ! -x "$OUTPUT/$CHROMIUM_PATH" ]]; then
    echo "Error: pinned Chromium executable is missing after extraction" >&2
    exit 2
fi
BUILD_ID=$(bash "$MANIFEST_TOOL" derive-build-id \
    --source-revision "$SOURCE_REVISION" \
    --version "$PLAYWRIGHT_VERSION" \
    --target "$TARGET" \
    --profile release-browser-runtime \
    --feature-contract node-playwright-chromium-v1)
bash "$MANIFEST_TOOL" generate \
    --root "$OUTPUT" \
    --source-root "$PROJECT_ROOT" \
    --output "$MANIFEST" \
    --build-id "$BUILD_ID" \
    --source-revision "$SOURCE_REVISION" \
    --version "$PLAYWRIGHT_VERSION" \
    --target "$TARGET" \
    --profile release-browser-runtime \
    --feature-contract node-playwright-chromium-v1 \
    --tree asset:browser-runtime:. \
    --input browser.runtime-lock=docs/release/browser-runtime-lock.v1.json \
    --input schema.browser-runtime-lock=docs/json-schema/ft-browser-runtime-lock-v1.json \
    --contract browser.runtime.schema=playwright-chromium.v1 \
    --contract browser.runtime.target="$TARGET" \
    --contract browser.runtime.root=runtime \
    --contract browser.node.path=runtime/bin/node \
    --contract browser.node.version="$NODE_VERSION" \
    --contract browser.playwright.module-path=runtime/node_modules/playwright/index.js \
    --contract browser.playwright.browsers-path=runtime/browsers \
    --contract browser.playwright.version="$PLAYWRIGHT_VERSION" \
    --contract browser.chromium.executable-path="$CHROMIUM_PATH" \
    --contract browser.chromium.revision="$CHROMIUM_REVISION" \
    --contract browser.protocol.version="$PROTOCOL_VERSION" \
    --contract browser.license.node-path=runtime/licenses/node.txt \
    --contract browser.license.playwright-path=runtime/licenses/playwright.txt \
    --contract browser.license.chromium-path=runtime/licenses/chrome-for-testing-NOTICE.txt \
    --contract browser.symlink-manifest.path="$SYMLINK_MANIFEST_REL" \
    --contract browser.disk-budget.bytes="$INSTALLED_BYTE_BUDGET"
bash "$MANIFEST_TOOL" verify --root "$OUTPUT" --manifest "$MANIFEST"

printf 'FT_BROWSER_RUNTIME_ASSEMBLY_SUCCESS target=%s playwright=%s chromium_revision=%s manifest=%s\n' \
    "$TARGET" "$PLAYWRIGHT_VERSION" "$CHROMIUM_REVISION" "$MANIFEST"
