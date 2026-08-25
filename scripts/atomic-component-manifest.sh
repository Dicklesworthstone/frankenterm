#!/usr/bin/env bash
set -euo pipefail

# Build and verify FrankenTerm's atomic component/asset manifest.
#
# The manifest is deliberately stricter than a checksum list:
#   * each executable must carry the same compile-time build identity;
#   * every regular file under the package root must be catalogued exactly;
#   * symlinks and special files are rejected;
#   * copied source assets can be required to match their source bytes; and
#   * the manifest has a content-derived identity over canonical JSON.
#
# Verification is completely offline.  Python 3 is the only runtime
# dependency; the verifier never opens a socket or resolves a remote name.

if ! command -v python3 >/dev/null 2>&1; then
  echo 'FT_ATOMIC_MANIFEST_ERROR code=python3_unavailable' \
    'remedy="install Python 3 and retry; verification did not run"' >&2
  exit 2
fi

exec python3 - "$@" <<'PY'
from __future__ import annotations

import argparse
import errno
import hashlib
import json
import os
import re
import stat
import sys
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, Iterator, NoReturn


SCHEMA_URI = "https://frankenterm.dev/schemas/ft-atomic-component-manifest/v1.json"
SCHEMA_VERSION = "ft.atomic_component_manifest.v1"
BUILD_IDENTITY_SCHEMA = "ft.atomic_build_identity.v1"
MAX_MANIFEST_BYTES = 64 * 1024 * 1024
MAX_FILES = 100_000
MAX_INVENTORY_ENTRIES = 200_000
MAX_INVENTORY_NAME_BYTES = 64 * 1024 * 1024
MAX_INPUTS = 10_000
MAX_CONTRACTS = 256
MAX_PATH_BYTES = 4096
MAX_TOKEN_BYTES = 255
MARKER_PREFIX = b"FT_ATOMIC_COMPONENT_IDENTITY_V1:"
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
SOURCE_REVISION_RE = re.compile(r"^[0-9a-f]{40}$")
TOKEN_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._+\-]*$")
ROLE_RE = re.compile(r"^[a-z0-9][a-z0-9._/\-]*$")
MARKER_RE = re.compile(
    rb"FT_ATOMIC_COMPONENT_IDENTITY_V1:"
    rb"([0-9a-f]{64}|unsealed):"
    rb"([A-Za-z0-9][A-Za-z0-9._+\-]*):"
    rb"([A-Za-z0-9][A-Za-z0-9._+\-]*):"
    rb"([A-Za-z0-9][A-Za-z0-9._+\-]*):"
    rb"([A-Za-z0-9][A-Za-z0-9._+\-]*);"
)
ALLOWED_KINDS = {
    "archive",
    "asset",
    "attestation",
    "checksum",
    "config",
    "executable",
    "font",
    "metadata",
    "schema",
    "signature",
    "verifier",
}
BROWSER_RUNTIME_SCHEMA = "playwright-chromium.v1"
BROWSER_RUNTIME_PATH_POLICY = "relative_utf8_browser_symlink_sidecar_v2"
BROWSER_RUNTIME_REQUIRED_CONTRACTS = {
    "browser.runtime.schema",
    "browser.runtime.target",
    "browser.runtime.root",
    "browser.node.path",
    "browser.node.version",
    "browser.playwright.module-path",
    "browser.playwright.browsers-path",
    "browser.playwright.version",
    "browser.chromium.executable-path",
    "browser.chromium.revision",
    "browser.protocol.version",
    "browser.license.node-path",
    "browser.license.playwright-path",
    "browser.license.chromium-path",
    "browser.symlink-manifest.path",
    "browser.disk-budget.bytes",
}
BROWSER_RUNTIME_PROVENANCE_CONTRACTS = {
    "browser.component.source-manifest-id",
    "browser.component.source-manifest-path",
}
TOP_LEVEL_KEYS = {
    "$schema",
    "schema_version",
    "manifest_id",
    "identity",
    "contracts",
    "files",
    "inputs",
    "inventory",
    "verification",
}
REMEDY_REBUILD = (
    "rebuild every FrankenTerm component in one clean build invocation, "
    "then regenerate the package manifest"
)
O_CLOEXEC = getattr(os, "O_CLOEXEC", 0)
O_DIRECTORY = getattr(os, "O_DIRECTORY", 0)
O_NOFOLLOW = getattr(os, "O_NOFOLLOW", 0)


class ManifestError(Exception):
    def __init__(self, code: str, message: str, **details: Any) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.details = details


def fail(code: str, message: str, **details: Any) -> NoReturn:
    raise ManifestError(code, message, **details)


def canonical_bytes(value: Any) -> bytes:
    return json.dumps(
        value,
        ensure_ascii=False,
        separators=(",", ":"),
        sort_keys=True,
    ).encode("utf-8")


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def stable_stat_identity(value: os.stat_result) -> tuple[int, int, int, int, int, int]:
    return (
        value.st_dev,
        value.st_ino,
        value.st_mode,
        value.st_size,
        value.st_mtime_ns,
        value.st_ctime_ns,
    )


def object_identity(value: os.stat_result) -> tuple[int, int, int]:
    return (value.st_dev, value.st_ino, value.st_mode)


def require_nofollow_platform() -> None:
    if O_NOFOLLOW == 0 or O_DIRECTORY == 0:
        fail(
            "nofollow_unavailable",
            "this platform cannot provide descriptor-anchored no-follow verification",
            remedy="verify on a supported POSIX host; do not substitute pathname-only hashing",
        )


def open_canonical_directory(path: Path, field: str) -> tuple[Path, int]:
    require_nofollow_platform()
    fd = -1
    try:
        absolute = path if path.is_absolute() else Path.cwd() / path
        canonical = absolute.parent.resolve(strict=True) / absolute.name
        fd = os.open(canonical, os.O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
        descriptor_stat = os.fstat(fd)
        pathname_stat = os.stat(canonical, follow_symlinks=False)
    except ManifestError:
        raise
    except OSError as exc:
        if fd >= 0:
            os.close(fd)
        fail("invalid_directory", f"cannot anchor {field}: {exc}", path=str(path), field=field)
    if not stat.S_ISDIR(descriptor_stat.st_mode) or object_identity(descriptor_stat) != object_identity(pathname_stat):
        os.close(fd)
        fail(
            "directory_identity_changed",
            f"{field} changed while its authority descriptor was opened",
            path=str(canonical),
            field=field,
        )
    return canonical, fd


StatIdentity = tuple[int, int, int, int, int, int]
ObjectIdentity = tuple[int, int, int]


@dataclass(frozen=True)
class ReadEvidence:
    sha256: str
    length: int
    executable: bool
    markers: tuple[dict[str, str], ...]
    payload: bytes | None
    identity: StatIdentity


@dataclass(frozen=True)
class InventorySnapshot:
    files: dict[str, StatIdentity]
    directories: dict[str, StatIdentity]
    symlinks: dict[str, str]
    invalid: tuple[str, ...]


@dataclass(frozen=True)
class AbsoluteRead:
    path: Path
    parent_identity: ObjectIdentity
    evidence: ReadEvidence


class AnchoredRoot:
    def __init__(self, path: Path, field: str) -> None:
        self.path, self.fd = open_canonical_directory(path, field)
        self.field = field
        self.identity = object_identity(os.fstat(self.fd))

    def close(self) -> None:
        if self.fd >= 0:
            os.close(self.fd)
            self.fd = -1

    def __enter__(self) -> "AnchoredRoot":
        return self

    def __exit__(self, _exc_type: Any, _exc: Any, _traceback: Any) -> None:
        self.close()

    def assert_path_identity(self) -> None:
        try:
            current = os.stat(self.path, follow_symlinks=False)
        except OSError as exc:
            fail(
                "root_identity_changed",
                f"{self.field} pathname no longer names its authority directory: {exc}",
                path=str(self.path),
                remedy="stop concurrent staging mutation and retry from an immutable directory",
            )
        if object_identity(current) != self.identity:
            fail(
                "root_identity_changed",
                f"{self.field} pathname changed after its authority descriptor was opened",
                path=str(self.path),
                remedy="stop concurrent staging mutation and retry from an immutable directory",
            )

    @contextmanager
    def open_directory(self, relative: str) -> Iterator[int]:
        relative = normalized_relative(relative, "directory path")
        current = os.dup(self.fd)
        try:
            for part in PurePosixPath(relative).parts:
                try:
                    next_fd = os.open(
                        part,
                        os.O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC,
                        dir_fd=current,
                    )
                except OSError as exc:
                    fail(
                        "required_tree_missing",
                        f"cannot open package directory {relative}: {exc}",
                        path=relative,
                    )
                os.close(current)
                current = next_fd
                if not stat.S_ISDIR(os.fstat(current).st_mode):
                    fail("required_tree_missing", f"package directory is not a directory: {relative}", path=relative)
            yield current
        finally:
            if current >= 0:
                os.close(current)

    @contextmanager
    def open_regular(self, relative: str) -> Iterator[tuple[int, os.stat_result]]:
        relative = normalized_relative(relative)
        parts = PurePosixPath(relative).parts
        current = os.dup(self.fd)
        file_fd = -1
        try:
            for part in parts[:-1]:
                try:
                    next_fd = os.open(
                        part,
                        os.O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC,
                        dir_fd=current,
                    )
                except OSError as exc:
                    fail(
                        "required_file_missing",
                        f"cannot traverse parent of {relative}: {exc}",
                        path=relative,
                    )
                os.close(current)
                current = next_fd
            try:
                file_fd = os.open(parts[-1], os.O_RDONLY | O_NOFOLLOW | O_CLOEXEC, dir_fd=current)
            except OSError as exc:
                fail(
                    "symlink_rejected" if exc.errno == errno.ELOOP else "required_file_missing",
                    f"cannot open required file {relative}: {exc}",
                    path=relative,
                    remedy="reassemble the package from the same complete build",
                )
            opened = os.fstat(file_fd)
            if not stat.S_ISREG(opened.st_mode):
                fail(
                    "special_file_rejected",
                    f"only regular files may be catalogued: {relative}",
                    path=relative,
                )
            self.maybe_test_swap_after_open(relative)
            yield file_fd, opened
        finally:
            if file_fd >= 0:
                os.close(file_fd)
            if current >= 0:
                os.close(current)

    def maybe_test_swap_after_open(self, relative: str) -> None:
        """Inject deterministic races for the adversarial E2E matrix only."""
        file_trigger = os.environ.get("FT_ATOMIC_MANIFEST_TEST_SWAP_FILE_AFTER_OPEN")
        parent_trigger = os.environ.get("FT_ATOMIC_MANIFEST_TEST_SWAP_PARENT_AFTER_OPEN")
        if relative not in {file_trigger, parent_trigger}:
            return
        self.perform_test_swap(
            relative,
            parent_swap=parent_trigger == relative,
            backup_prefix=".ft-atomic-opened-",
        )
        os.environ.pop(
            "FT_ATOMIC_MANIFEST_TEST_SWAP_PARENT_AFTER_OPEN"
            if parent_trigger == relative
            else "FT_ATOMIC_MANIFEST_TEST_SWAP_FILE_AFTER_OPEN",
            None,
        )

    def maybe_test_swap_after_precommit_scan(self) -> None:
        relative = os.environ.get("FT_ATOMIC_MANIFEST_TEST_SWAP_AFTER_PRECOMMIT_SCAN")
        if relative is None:
            return
        relative = normalized_relative(relative, "test race path")
        self.perform_test_swap(
            relative,
            parent_swap=False,
            backup_prefix=".ft-atomic-precommit-",
        )
        os.environ.pop("FT_ATOMIC_MANIFEST_TEST_SWAP_AFTER_PRECOMMIT_SCAN", None)

    def perform_test_swap(
        self,
        relative: str,
        parent_swap: bool,
        backup_prefix: str,
    ) -> None:
        if os.environ.get("FT_ATOMIC_MANIFEST_TEST_MODE") != "1":
            fail(
                "test_hook_forbidden",
                "atomic-manifest race injection is restricted to the explicit test harness",
            )
        replacement_raw = os.environ.get("FT_ATOMIC_MANIFEST_TEST_SWAP_REPLACEMENT")
        if not replacement_raw:
            fail("invalid_test_hook", "race injection requires a replacement path")
        replacement = Path(replacement_raw).resolve(strict=True)
        relative_path = PurePosixPath(relative)
        parent_parts = relative_path.parts[:-1]
        parent_fd = os.dup(self.fd)
        try:
            for part in parent_parts[:-1] if parent_swap else parent_parts:
                next_fd = os.open(
                    part,
                    os.O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC,
                    dir_fd=parent_fd,
                )
                os.close(parent_fd)
                parent_fd = next_fd
            if parent_swap:
                if not parent_parts:
                    fail("invalid_test_hook", "parent-swap target must have a package subdirectory")
                name = parent_parts[-1]
                backup = f"{backup_prefix}{name}"
                os.rename(name, backup, src_dir_fd=parent_fd, dst_dir_fd=parent_fd)
                os.symlink(str(replacement), name, target_is_directory=True, dir_fd=parent_fd)
            else:
                name = relative_path.name
                backup = f"{backup_prefix}{name}"
                os.rename(name, backup, src_dir_fd=parent_fd, dst_dir_fd=parent_fd)
                os.symlink(str(replacement), name, dir_fd=parent_fd)
        except ManifestError:
            raise
        except OSError as exc:
            fail("test_hook_failed", f"deterministic race injection failed: {exc}", path=relative)
        finally:
            os.close(parent_fd)

    def read_regular(self, relative: str, scan_markers: bool = False) -> ReadEvidence:
        with self.open_regular(relative) as (fd, before):
            digest = hashlib.sha256()
            length = 0
            carry = b""
            marker_records: list[dict[str, str]] = []
            marker_prefix_count = 0
            retained = bytearray() if before.st_size <= MAX_MANIFEST_BYTES else None
            while True:
                chunk = os.read(fd, 1024 * 1024)
                if not chunk:
                    break
                previous_length = length
                digest.update(chunk)
                length += len(chunk)
                if retained is not None:
                    retained.extend(chunk)
                if scan_markers:
                    payload = carry + chunk
                    payload_offset = previous_length - len(carry)
                    prefix_offset = 0
                    while True:
                        prefix_offset = payload.find(MARKER_PREFIX, prefix_offset)
                        if prefix_offset < 0:
                            break
                        absolute_end = payload_offset + prefix_offset + len(MARKER_PREFIX)
                        if absolute_end > previous_length:
                            marker_prefix_count += 1
                            if marker_prefix_count > 1:
                                fail(
                                    "duplicate_component_identity_marker",
                                    f"component {relative} contains more than one raw atomic identity marker",
                                    path=relative,
                                )
                        prefix_offset += 1
                    for match in MARKER_RE.finditer(payload):
                        absolute_end = payload_offset + match.end()
                        if absolute_end > previous_length:
                            marker_records.append(decode_marker(match))
                    carry = payload[-2048:]
            after = os.fstat(fd)
            if stable_stat_identity(before) != stable_stat_identity(after) or length != before.st_size:
                fail(
                    "file_changed_during_read",
                    f"file changed while being hashed through its authority descriptor: {relative}",
                    path=relative,
                    remedy="stop concurrent staging mutation and rebuild in a fresh immutable directory",
                )
            return ReadEvidence(
                sha256=digest.hexdigest(),
                length=length,
                executable=bool(before.st_mode & 0o111),
                markers=tuple(marker_records),
                payload=bytes(retained) if retained is not None else None,
                identity=stable_stat_identity(before),
            )

    def scan_inventory(self, ignored: set[str]) -> InventorySnapshot:
        files: dict[str, StatIdentity] = {}
        directories: dict[str, StatIdentity] = {}
        symlinks: dict[str, str] = {}
        invalid: list[str] = []
        pending: list[tuple[int, str]] = [(os.dup(self.fd), "")]
        try:
            while pending:
                directory_fd, prefix = pending.pop()
                try:
                    try:
                        entries = []
                        name_bytes = 0
                        with os.scandir(directory_fd) as iterator:
                            for entry in iterator:
                                if len(entries) >= MAX_INVENTORY_ENTRIES:
                                    fail(
                                        "inventory_too_large",
                                        "package directory exceeds the bounded pre-sort entry limit",
                                        path=prefix or ".",
                                        maximum_entries=MAX_INVENTORY_ENTRIES,
                                    )
                                try:
                                    encoded_name = entry.name.encode("utf-8")
                                except UnicodeError:
                                    fail(
                                        "invalid_inventory_path",
                                        "package contains a non-UTF-8 inventory name",
                                        path=prefix or ".",
                                    )
                                name_bytes += len(encoded_name)
                                if name_bytes > MAX_INVENTORY_NAME_BYTES:
                                    fail(
                                        "inventory_too_large",
                                        "package directory exceeds the bounded pre-sort name-byte limit",
                                        path=prefix or ".",
                                        maximum_name_bytes=MAX_INVENTORY_NAME_BYTES,
                                    )
                                entries.append(entry)
                        entries.sort(key=lambda entry: entry.name)
                    except OSError as exc:
                        fail(
                            "inventory_scan_failed",
                            f"cannot scan package inventory: {exc}",
                            path=prefix or ".",
                        )
                    for entry in entries:
                        relative = f"{prefix}/{entry.name}" if prefix else entry.name
                        try:
                            normalized_relative(relative, "inventory path")
                        except (ManifestError, UnicodeError):
                            fail(
                                "invalid_inventory_path",
                                "package contains a non-portable or overlong path",
                                path=ascii(relative)[:512],
                            )
                        try:
                            observed = entry.stat(follow_symlinks=False)
                        except OSError:
                            invalid.append(relative)
                            continue
                        if stat.S_ISDIR(observed.st_mode):
                            try:
                                child_fd = os.open(
                                    entry.name,
                                    os.O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC,
                                    dir_fd=directory_fd,
                                )
                            except OSError:
                                invalid.append(relative)
                                continue
                            try:
                                opened = os.fstat(child_fd)
                            except OSError:
                                os.close(child_fd)
                                invalid.append(relative)
                                continue
                            if object_identity(observed) != object_identity(opened):
                                os.close(child_fd)
                                fail(
                                    "directory_identity_changed",
                                    f"package directory changed during descriptor-anchored scan: {relative}",
                                    path=relative,
                                )
                            directories[relative] = stable_stat_identity(opened)
                            pending.append((child_fd, relative))
                        elif stat.S_ISREG(observed.st_mode):
                            if relative not in ignored:
                                files[relative] = stable_stat_identity(observed)
                        elif stat.S_ISLNK(observed.st_mode):
                            try:
                                target = os.readlink(entry.name, dir_fd=directory_fd)
                            except (OSError, UnicodeError):
                                invalid.append(relative)
                                continue
                            if not target or "\x00" in target or len(target.encode("utf-8")) > MAX_PATH_BYTES:
                                invalid.append(relative)
                                continue
                            symlinks[relative] = target
                        else:
                            invalid.append(relative)
                        if len(files) > MAX_FILES or len(files) + len(directories) + len(symlinks) > MAX_INVENTORY_ENTRIES:
                            fail(
                                "inventory_too_large",
                                "package inventory exceeds the bounded verifier limit",
                                maximum_files=MAX_FILES,
                                maximum_entries=MAX_INVENTORY_ENTRIES,
                            )
                finally:
                    os.close(directory_fd)
        finally:
            for directory_fd, _prefix in pending:
                os.close(directory_fd)
        return InventorySnapshot(files, directories, symlinks, tuple(sorted(invalid)))

    def assert_regular_identity(self, relative: str, expected: StatIdentity) -> None:
        with self.open_regular(relative) as (_fd, observed):
            actual = stable_stat_identity(observed)
        if actual != expected:
            fail(
                "file_identity_changed",
                f"file identity changed after descriptor-bound read: {relative}",
                path=relative,
                remedy="stop concurrent staging mutation and retry from an immutable directory",
            )


def read_absolute_regular(path: Path, field: str) -> AbsoluteRead:
    try:
        absolute = path if path.is_absolute() else Path.cwd() / path
        canonical = absolute.parent.resolve(strict=True) / absolute.name
    except OSError as exc:
        fail("required_file_missing", f"cannot resolve {field}: {exc}", path=str(path))
    parent = AnchoredRoot(canonical.parent, f"{field} parent")
    try:
        evidence = parent.read_regular(canonical.name)
        parent.assert_path_identity()
    finally:
        parent.close()
    if evidence.length > MAX_MANIFEST_BYTES or evidence.payload is None:
        fail(
            "manifest_too_large",
            f"{field} exceeds the bounded offline-verification size",
            path=str(path),
            bytes=evidence.length,
            maximum_bytes=MAX_MANIFEST_BYTES,
        )
    try:
        current = os.stat(canonical, follow_symlinks=False)
    except OSError as exc:
        fail("file_identity_changed", f"{field} pathname changed after descriptor-bound read: {exc}")
    if stable_stat_identity(current) != evidence.identity:
        fail("file_identity_changed", f"{field} pathname changed after descriptor-bound read", path=str(canonical))
    return AbsoluteRead(canonical, parent.identity, evidence)


def assert_absolute_read_current(value: AbsoluteRead, field: str) -> None:
    try:
        parent = os.stat(value.path.parent, follow_symlinks=False)
        current = os.stat(value.path, follow_symlinks=False)
    except OSError as exc:
        fail("file_identity_changed", f"{field} pathname changed after descriptor-bound read: {exc}")
    if object_identity(parent) != value.parent_identity or stable_stat_identity(current) != value.evidence.identity:
        fail("file_identity_changed", f"{field} pathname changed after descriptor-bound read", path=str(value.path))


def write_new_absolute(path: Path, payload: bytes) -> None:
    try:
        canonical_parent = path.parent.resolve(strict=True)
    except OSError as exc:
        fail("manifest_write_failed", f"cannot resolve manifest output parent: {exc}", path=str(path))
    parent = AnchoredRoot(canonical_parent, "manifest output parent")
    fd = -1
    try:
        try:
            fd = os.open(
                path.name,
                os.O_WRONLY | os.O_CREAT | os.O_EXCL | O_NOFOLLOW | O_CLOEXEC,
                0o644,
                dir_fd=parent.fd,
            )
        except OSError as exc:
            fail(
                "output_exists" if exc.errno == errno.EEXIST else "manifest_write_failed",
                f"cannot create authority manifest without overwrite: {exc}",
                path=str(path),
                remedy="use a fresh staging/output directory",
            )
        written = 0
        while written < len(payload):
            count = os.write(fd, payload[written:])
            if count <= 0:
                fail("manifest_write_failed", "manifest output descriptor made no forward progress")
            written += count
        os.fsync(fd)
        after = os.fstat(fd)
        if not stat.S_ISREG(after.st_mode) or after.st_size != len(payload):
            fail("manifest_write_failed", "manifest output descriptor did not retain the complete regular file")
        try:
            pathname = os.stat(path.name, dir_fd=parent.fd, follow_symlinks=False)
        except OSError as exc:
            fail("manifest_write_failed", f"manifest output pathname changed before commit: {exc}")
        if stable_stat_identity(pathname) != stable_stat_identity(after):
            fail("manifest_write_failed", "manifest output pathname does not name the committed descriptor")
        os.fsync(parent.fd)
        parent.assert_path_identity()
    finally:
        if fd >= 0:
            os.close(fd)
        parent.close()


def reject_duplicate_json_pairs(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            fail(
                "duplicate_json_key",
                f"manifest contains duplicate JSON key {key!r}",
                key=key,
                remedy="regenerate the manifest with the canonical generator",
            )
        result[key] = value
    return result


def load_json_strict(payload: bytes, display_path: str) -> dict[str, Any]:
    try:
        value = json.loads(
            payload.decode("utf-8"),
            object_pairs_hook=reject_duplicate_json_pairs,
            parse_constant=lambda token: fail(
                "invalid_json_number",
                f"manifest contains non-standard JSON number {token}",
            ),
        )
    except ManifestError:
        raise
    except (UnicodeError, json.JSONDecodeError, RecursionError) as exc:
        fail(
            "manifest_unreadable",
            f"cannot read manifest {display_path}: {exc}",
            path=display_path,
            remedy="obtain the intact manifest emitted with this exact package",
        )
    if not isinstance(value, dict):
        fail(
            "manifest_not_object",
            "manifest root must be a JSON object",
            path=display_path,
            remedy="regenerate the manifest with the canonical generator",
        )
    return value


def validate_token(name: str, value: str) -> str:
    if len(value.encode("utf-8")) > MAX_TOKEN_BYTES or not TOKEN_RE.fullmatch(value):
        fail(
            "invalid_identity_token",
            f"{name} contains unsupported characters",
            field=name,
            actual=value,
            remedy="use a non-empty ASCII token containing letters, digits, dot, plus, underscore, or hyphen",
        )
    return value


def validate_role(name: str, value: str) -> str:
    if len(value.encode("utf-8")) > MAX_PATH_BYTES or not ROLE_RE.fullmatch(value):
        fail(
            "invalid_role",
            f"{name} is not a canonical role token",
            field=name,
            actual=value,
        )
    return value


def validate_build_id(value: str) -> str:
    if not SHA256_RE.fullmatch(value):
        fail(
            "invalid_build_id",
            "build identity must be 64 lowercase hexadecimal characters",
            actual=value,
            remedy="derive it with atomic-component-manifest.sh derive-build-id",
        )
    if value == "0" * 64:
        fail(
            "invalid_build_id",
            "build identity must not be the all-zero sentinel",
            actual=value,
            remedy="derive it with atomic-component-manifest.sh derive-build-id",
        )
    return value


def validate_source_revision(value: str) -> str:
    if not SOURCE_REVISION_RE.fullmatch(value):
        fail(
            "invalid_source_revision",
            "source revision must be a full 40-character lowercase Git object id",
            actual=value,
            remedy="use git rev-parse HEAD from the exact clean source snapshot",
        )
    return value


def normalized_relative(value: str, field: str = "path") -> str:
    if (
        not value
        or len(value.encode("utf-8")) > MAX_PATH_BYTES
        or "\\" in value
        or "\x00" in value
    ):
        fail(
            "invalid_relative_path",
            f"{field} must be a non-empty portable POSIX relative path",
            field=field,
            actual=value,
        )
    pure = PurePosixPath(value)
    if pure.is_absolute() or value.startswith("/") or any(part in {"", ".", ".."} for part in pure.parts):
        fail(
            "invalid_relative_path",
            f"{field} must not be absolute or contain empty, dot, or parent segments",
            field=field,
            actual=value,
        )
    normalized = pure.as_posix()
    if normalized != value:
        fail(
            "noncanonical_relative_path",
            f"{field} is not in canonical POSIX form",
            field=field,
            actual=value,
            expected=normalized,
        )
    return normalized


def split_colon(value: str, fields: int, option: str) -> list[str]:
    parts = value.split(":", fields - 1)
    if len(parts) != fields:
        fail(
            "invalid_option_value",
            f"{option} expects {fields} colon-separated fields",
            option=option,
            actual=value,
        )
    return parts


def split_equals(value: str, option: str) -> tuple[str, str]:
    if "=" not in value:
        fail(
            "invalid_option_value",
            f"{option} expects KEY=VALUE",
            option=option,
            actual=value,
        )
    key, raw = value.split("=", 1)
    if not key or not raw:
        fail(
            "invalid_option_value",
            f"{option} expects non-empty KEY=VALUE",
            option=option,
            actual=value,
        )
    return key, raw


def derive_build_identity(
    source_revision: str,
    version: str,
    target: str,
    profile: str,
    feature_contract: str,
) -> str:
    value = {
        "feature_contract": validate_token("feature_contract", feature_contract),
        "profile": validate_token("profile", profile),
        "schema_version": BUILD_IDENTITY_SCHEMA,
        "source_revision": validate_source_revision(source_revision),
        "target": validate_token("target", target),
        "version": validate_token("version", version),
    }
    return sha256_bytes(canonical_bytes(value))


def expected_component_marker(identity: dict[str, str], component: str) -> dict[str, str]:
    component = validate_token("component", component)
    return {
        "build_id": identity["build_id"],
        "component": component,
        "target": identity["target"],
        "profile": identity["profile"],
        "version": identity["version"],
    }


def decode_marker(match: re.Match[bytes]) -> dict[str, str]:
    return {
        "build_id": match.group(1).decode("ascii"),
        "component": match.group(2).decode("ascii"),
        "target": match.group(3).decode("ascii"),
        "profile": match.group(4).decode("ascii"),
        "version": match.group(5).decode("ascii"),
    }


def verify_component_markers(
    found: tuple[dict[str, str], ...],
    relative: str,
    component: str,
    identity: dict[str, str],
) -> None:
    expected = expected_component_marker(identity, component)
    if list(found) != [expected]:
        fail(
            "component_identity_mismatch" if found else "component_identity_missing",
            f"component {relative} does not carry the required atomic build identity",
            path=relative,
            component=component,
            expected={
                "build_id": identity["build_id"],
                "target": identity["target"],
                "profile": identity["profile"],
                "version": identity["version"],
            },
            found=list(found),
            remedy=REMEDY_REBUILD,
        )


@dataclass(frozen=True)
class EntrySpec:
    kind: str
    role: str
    path: str
    component: str | None


def parse_entry(value: str) -> EntrySpec:
    parts = value.split(":", 3)
    if len(parts) not in {3, 4}:
        fail(
            "invalid_entry_spec",
            "--entry expects KIND:ROLE:PATH[:COMPONENT]",
            actual=value,
        )
    kind, role, relative = parts[:3]
    component = parts[3] if len(parts) == 4 else None
    if kind not in ALLOWED_KINDS:
        fail("invalid_file_kind", f"unsupported file kind {kind!r}", actual=kind)
    validate_role("role", role)
    normalized_relative(relative)
    if component is not None:
        validate_token("component", component)
        if kind != "executable":
            fail(
                "identity_on_non_executable",
                "only executable entries may declare a component identity",
                actual=value,
            )
    elif kind == "executable":
        fail(
            "executable_component_required",
            "every executable entry must declare its compile-time component identity",
            actual=value,
            remedy="use --entry KIND:ROLE:PATH:COMPONENT for each executable",
        )
    return EntrySpec(kind, role, relative, component)


def parse_tree(value: str) -> tuple[str, str, str]:
    kind, role_prefix, relative = split_colon(value, 3, "--tree")
    if kind not in ALLOWED_KINDS:
        fail("invalid_file_kind", f"unsupported file kind {kind!r}", actual=kind)
    if kind == "executable":
        fail(
            "executable_tree_forbidden",
            "executable trees cannot assign a sound per-file component identity",
            actual=value,
            remedy="catalog each executable with --entry KIND:ROLE:PATH:COMPONENT",
        )
    validate_role("role_prefix", role_prefix)
    if relative != ".":
        normalized_relative(relative)
    return kind, role_prefix, relative


def add_entry(
    specs: dict[str, EntrySpec],
    spec: EntrySpec,
    source: str,
) -> None:
    if spec.path in specs:
        fail(
            "duplicate_catalog_path",
            f"package path {spec.path} is catalogued more than once",
            path=spec.path,
            first_role=specs[spec.path].role,
            second_role=spec.role,
            source=source,
        )
    specs[spec.path] = spec


def expand_specs(
    root: AnchoredRoot,
    inventory: InventorySnapshot,
    entries: list[str],
    trees: list[str],
    optional_trees: list[str],
) -> dict[str, EntrySpec]:
    specs: dict[str, EntrySpec] = {}
    for value in entries:
        add_entry(specs, parse_entry(value), "--entry")
    for value, optional in [(value, False) for value in trees] + [
        (value, True) for value in optional_trees
    ]:
        kind, prefix, relative = parse_tree(value)
        if relative != ".":
            try:
                with root.open_directory(relative):
                    pass
            except ManifestError as exc:
                if optional and exc.code == "required_tree_missing":
                    continue
                raise
        tree_prefix = "" if relative == "." else relative + "/"
        for package_rel in sorted(path for path in inventory.files if path.startswith(tree_prefix)):
            # The package path already gives each tree member a unique,
            # case-preserving identity. Keep the semantic role at the
            # declared prefix so arbitrary filenames do not need rewriting.
            add_entry(specs, EntrySpec(kind, prefix, package_rel, None), "--tree")
            if len(specs) > MAX_FILES:
                fail(
                    "catalog_too_large",
                    "package catalog exceeds the bounded verifier limit",
                    maximum_files=MAX_FILES,
                )
    return specs


def output_relative_to_root(root: Path, output: Path) -> str | None:
    try:
        absolute = output if output.is_absolute() else Path.cwd() / output
        canonical_output = absolute.parent.resolve(strict=True) / absolute.name
        return canonical_output.relative_to(root).as_posix()
    except (OSError, ValueError):
        return None


def validate_identity_mapping(value: Any) -> dict[str, str]:
    if not isinstance(value, dict):
        fail("invalid_identity", "identity must be a JSON object")
    expected_keys = {
        "build_id",
        "source_revision",
        "version",
        "target",
        "profile",
        "feature_contract",
    }
    if set(value) != expected_keys or not all(isinstance(item, str) for item in value.values()):
        fail(
            "invalid_identity",
            "identity has an unexpected field set or non-string value",
            expected=sorted(expected_keys),
            actual=sorted(value) if isinstance(value, dict) else None,
        )
    derived = derive_build_identity(
        value["source_revision"],
        value["version"],
        value["target"],
        value["profile"],
        value["feature_contract"],
    )
    validate_build_id(value["build_id"])
    if value["build_id"] != derived:
        fail(
            "build_identity_derivation_mismatch",
            "manifest build identity does not match its declared source/build fields",
            expected=derived,
            actual=value["build_id"],
            remedy=REMEDY_REBUILD,
        )
    return value


def require_inventory_identity(
    evidence: ReadEvidence,
    expected: StatIdentity,
    relative: str,
) -> None:
    if evidence.identity != expected:
        fail(
            "package_inventory_changed",
            f"package file identity changed between inventory and read: {relative}",
            path=relative,
            remedy="stop concurrent staging mutation and retry from an immutable directory",
        )


def build_file_record(
    root: AnchoredRoot,
    spec: EntrySpec,
    identity: dict[str, str],
    expected_identity: StatIdentity,
) -> dict[str, Any]:
    evidence = root.read_regular(spec.path, scan_markers=spec.component is not None)
    require_inventory_identity(evidence, expected_identity, spec.path)
    if spec.component is not None:
        verify_component_markers(evidence.markers, spec.path, spec.component, identity)
    record: dict[str, Any] = {
        "bytes": evidence.length,
        "executable": evidence.executable,
        "kind": spec.kind,
        "path": spec.path,
        "role": spec.role,
        "sha256": evidence.sha256,
    }
    if spec.component is not None:
        record["component"] = spec.component
    return record


def parse_contracts(values: list[str]) -> dict[str, str]:
    if len(values) > MAX_CONTRACTS:
        fail(
            "too_many_contracts",
            "contract count exceeds the bounded verifier limit",
            maximum_contracts=MAX_CONTRACTS,
        )
    contracts: dict[str, str] = {}
    for value in values:
        key, raw = split_equals(value, "--contract")
        validate_role("contract_key", key)
        if key in contracts:
            fail("duplicate_contract_key", f"contract key {key!r} is repeated", key=key)
        if not raw or len(raw.encode("utf-8")) > MAX_PATH_BYTES or any(ord(char) < 0x20 for char in raw):
            fail("invalid_contract_value", f"invalid contract value for {key!r}", key=key)
        contracts[key] = raw
    return dict(sorted(contracts.items()))


def read_source(
    source_root: AnchoredRoot,
    relative: str,
    cache: dict[str, ReadEvidence],
) -> ReadEvidence:
    if relative not in cache:
        cache[relative] = source_root.read_regular(relative)
    return cache[relative]


def build_inputs(
    source_root: AnchoredRoot,
    values: list[str],
    source_cache: dict[str, ReadEvidence],
) -> list[dict[str, Any]]:
    if len(values) > MAX_INPUTS:
        fail(
            "too_many_inputs",
            "input count exceeds the bounded verifier limit",
            maximum_inputs=MAX_INPUTS,
        )
    records: list[dict[str, Any]] = []
    seen_roles: set[str] = set()
    seen_paths: set[str] = set()
    for value in values:
        role, relative = split_equals(value, "--input")
        validate_role("input_role", role)
        relative = normalized_relative(relative, "input path")
        if role in seen_roles or relative in seen_paths:
            fail(
                "duplicate_input",
                "input roles and paths must be unique",
                role=role,
                path=relative,
            )
        evidence = read_source(source_root, relative, source_cache)
        records.append(
            {"bytes": evidence.length, "path": relative, "role": role, "sha256": evidence.sha256}
        )
        seen_roles.add(role)
        seen_paths.add(relative)
    return sorted(records, key=lambda record: record["path"])


def verify_source_matches(
    source_root: AnchoredRoot,
    values: list[str],
    catalogued: dict[str, dict[str, Any]],
    source_cache: dict[str, ReadEvidence],
) -> None:
    for value in values:
        package_rel, source_rel = split_equals(value, "--source-match")
        package_rel = normalized_relative(package_rel, "package path")
        source_rel = normalized_relative(source_rel, "source path")
        if package_rel not in catalogued:
            fail(
                "source_match_uncatalogued",
                "source-match package path must also be catalogued",
                path=package_rel,
            )
        package = catalogued[package_rel]
        source = read_source(source_root, source_rel, source_cache)
        if package["sha256"] != source.sha256 or package["bytes"] != source.length:
            fail(
                "source_asset_mismatch",
                f"packaged asset {package_rel} does not match source asset {source_rel}",
                package_path=package_rel,
                source_path=source_rel,
                expected_sha256=source.sha256,
                actual_sha256=package["sha256"],
                expected_bytes=source.length,
                actual_bytes=package["bytes"],
                remedy="copy the declared source asset into a fresh package staging directory and regenerate",
            )


def manifest_without_id(manifest: dict[str, Any]) -> dict[str, Any]:
    payload = dict(manifest)
    payload.pop("manifest_id", None)
    return payload


def inventory_change_details(
    initial: InventorySnapshot,
    final: InventorySnapshot,
    allowed_directory_metadata_changes: set[str] | None = None,
) -> dict[str, list[str]]:
    allowed = allowed_directory_metadata_changes or set()
    initial_files = set(initial.files)
    final_files = set(final.files)
    initial_directories = set(initial.directories)
    final_directories = set(final.directories)
    initial_symlinks = set(initial.symlinks)
    final_symlinks = set(final.symlinks)
    return {
        "added_files": sorted(final_files - initial_files),
        "removed_files": sorted(initial_files - final_files),
        "replaced_files": sorted(
            path
            for path in initial_files & final_files
            if initial.files[path] != final.files[path]
        ),
        "added_directories": sorted(final_directories - initial_directories),
        "removed_directories": sorted(initial_directories - final_directories),
        "replaced_directories": sorted(
            path
            for path in initial_directories & final_directories
            if initial.directories[path] != final.directories[path]
            and not (
                path in allowed
                and initial.directories[path][:3] == final.directories[path][:3]
            )
        ),
        "added_symlinks": sorted(final_symlinks - initial_symlinks),
        "removed_symlinks": sorted(initial_symlinks - final_symlinks),
        "retargeted_symlinks": sorted(
            path
            for path in initial_symlinks & final_symlinks
            if initial.symlinks[path] != final.symlinks[path]
        ),
        "invalid_entries": list(final.invalid),
    }


def require_stable_inventory(
    initial: InventorySnapshot,
    final: InventorySnapshot,
    allowed_directory_metadata_changes: set[str] | None = None,
) -> None:
    details = inventory_change_details(initial, final, allowed_directory_metadata_changes)
    if any(details.values()):
        fail(
            "package_inventory_changed",
            "package inventory changed during descriptor-bound verification",
            **details,
            remedy="stop concurrent staging mutation and retry from an immutable directory",
        )


def output_parent_inventory_exception(root: Path, output: Path) -> set[str]:
    try:
        relative = output.parent.relative_to(root).as_posix()
    except ValueError:
        return set()
    return set() if relative == "." else {relative}


def generate(args: argparse.Namespace) -> dict[str, Any]:
    output = Path(args.output)
    if not output.is_absolute():
        output = Path.cwd() / output
    try:
        output = output.parent.resolve(strict=True) / output.name
    except OSError as exc:
        fail("manifest_write_failed", f"cannot resolve manifest output parent: {exc}", path=str(output))

    identity = {
        "build_id": validate_build_id(args.build_id),
        "feature_contract": validate_token("feature_contract", args.feature_contract),
        "profile": validate_token("profile", args.profile),
        "source_revision": validate_source_revision(args.source_revision),
        "target": validate_token("target", args.target),
        "version": validate_token("version", args.version),
    }
    validate_identity_mapping(identity)
    with AnchoredRoot(Path(args.root), "package root") as root, AnchoredRoot(
        Path(args.source_root), "source root"
    ) as source_root:
        output_rel = output_relative_to_root(root.path, output)
        ignored = {output_rel} if output_rel is not None else set()
        initial = root.scan_inventory(ignored)
        if initial.invalid:
            fail(
                "non_regular_inventory_entry",
                "package contains symlink or special filesystem entries",
                paths=list(initial.invalid),
                remedy="stage only regular files in a fresh package root",
            )
        contracts = parse_contracts(args.contract)
        if initial.symlinks and contracts.get("browser.runtime.schema") != BROWSER_RUNTIME_SCHEMA:
            fail(
                "non_regular_inventory_entry",
                "package contains symlinks outside the browser runtime contract",
                paths=sorted(initial.symlinks),
            )
        specs = expand_specs(root, initial, args.entry, args.tree, args.optional_tree)
        if not specs:
            fail("empty_catalog", "at least one package file must be catalogued")
        if len(specs) > MAX_FILES:
            fail("catalog_too_large", "package catalog exceeds the bounded verifier limit", maximum_files=MAX_FILES)

        expected_files = set(specs)
        actual_files = set(initial.files)
        if actual_files != expected_files:
            fail(
                "package_inventory_mismatch",
                "package inventory is not exactly the declared catalog",
                missing=sorted(expected_files - actual_files),
                uncatalogued=sorted(actual_files - expected_files),
                remedy="remove accidental companions or explicitly classify every intentional package file",
            )

        file_records = [
            build_file_record(root, specs[path], identity, initial.files[path])
            for path in sorted(specs)
        ]
        catalogued = {record["path"]: record for record in file_records}
        source_cache: dict[str, ReadEvidence] = {}
        verify_source_matches(source_root, args.source_match, catalogued, source_cache)
        input_records = build_inputs(source_root, args.input, source_cache)
        for relative, evidence in source_cache.items():
            source_root.assert_regular_identity(relative, evidence.identity)
        source_root.assert_path_identity()
        precommit = root.scan_inventory(ignored)
        require_stable_inventory(initial, precommit)
        root.assert_path_identity()
        root.maybe_test_swap_after_precommit_scan()

        total_bytes = sum(record["bytes"] for record in file_records)
        manifest: dict[str, Any] = {
            "$schema": SCHEMA_URI,
            "schema_version": SCHEMA_VERSION,
            "identity": identity,
            "contracts": contracts,
            "files": file_records,
            "inputs": input_records,
            "inventory": {
                "file_count": len(file_records),
                "mode": "exact",
                "total_bytes": total_bytes,
            },
            "verification": {
                "algorithm": "sha256",
                "offline": True,
                "path_policy": (
                    BROWSER_RUNTIME_PATH_POLICY
                    if contracts.get("browser.runtime.schema") == BROWSER_RUNTIME_SCHEMA
                    else "relative_utf8_no_symlink_v1"
                ),
            },
        }
        validate_browser_runtime_contract(identity, contracts, file_records)
        validate_browser_runtime_symlinks(root, manifest, initial.symlinks)
        verify_browser_runtime_provenance(root, manifest)
        verify_browser_runtime_quarantine(root.path, manifest)
        manifest["manifest_id"] = "sha256:" + sha256_bytes(canonical_bytes(manifest))
        payload = (json.dumps(manifest, ensure_ascii=False, indent=2, sort_keys=True) + "\n").encode("utf-8")
        write_new_absolute(output, payload)
        committed_manifest = read_absolute_regular(output, "committed manifest")
        if committed_manifest.evidence.payload != payload:
            fail(
                "manifest_commit_mismatch",
                "committed manifest bytes differ from the descriptor-bound generated payload",
                path=str(output),
            )
        for relative, evidence in source_cache.items():
            source_root.assert_regular_identity(relative, evidence.identity)
        source_root.assert_path_identity()
        postcommit = root.scan_inventory(ignored)
        require_stable_inventory(
            initial,
            postcommit,
            output_parent_inventory_exception(root.path, output),
        )
        root.assert_path_identity()
        assert_absolute_read_current(committed_manifest, "committed manifest")
    return {
        "ok": True,
        "operation": "generate",
        "manifest": str(output),
        "manifest_id": manifest["manifest_id"],
        "build_id": identity["build_id"],
        "file_count": len(file_records),
        "total_bytes": total_bytes,
    }


def require_exact_keys(value: dict[str, Any], expected: set[str], field: str) -> None:
    if set(value) != expected:
        fail(
            "schema_field_mismatch",
            f"{field} has an unexpected field set",
            field=field,
            missing=sorted(expected - set(value)),
            extra=sorted(set(value) - expected),
        )


def validate_nonnegative_int(value: Any, field: str) -> int:
    if type(value) is not int or value < 0:
        fail("schema_type_mismatch", f"{field} must be a non-negative integer", field=field, actual=value)
    return value


def path_is_or_descendant(path: str, directory: str) -> bool:
    return path == directory or path.startswith(directory + "/")


def validate_browser_runtime_contract(
    identity: dict[str, str],
    contracts: dict[str, str],
    files: list[dict[str, Any]],
) -> None:
    schema = contracts.get("browser.runtime.schema")
    browser_keys = {key for key in contracts if key.startswith("browser.")}
    if schema is None:
        if browser_keys:
            fail(
                "browser_runtime_contract_incomplete",
                "browser-prefixed contracts require browser.runtime.schema",
                keys=sorted(browser_keys),
            )
        return
    if schema != BROWSER_RUNTIME_SCHEMA:
        fail(
            "browser_runtime_schema_mismatch",
            "browser runtime contract schema is unsupported",
            expected=BROWSER_RUNTIME_SCHEMA,
            actual=schema,
        )

    missing = BROWSER_RUNTIME_REQUIRED_CONTRACTS - browser_keys
    provenance_present = browser_keys & BROWSER_RUNTIME_PROVENANCE_CONTRACTS
    if provenance_present and provenance_present != BROWSER_RUNTIME_PROVENANCE_CONTRACTS:
        missing |= BROWSER_RUNTIME_PROVENANCE_CONTRACTS - provenance_present
    allowed = BROWSER_RUNTIME_REQUIRED_CONTRACTS | BROWSER_RUNTIME_PROVENANCE_CONTRACTS
    extra = browser_keys - allowed
    if missing or extra:
        fail(
            "browser_runtime_contract_field_mismatch",
            "browser runtime contract has an unexpected field set",
            missing=sorted(missing),
            extra=sorted(extra),
        )
    if contracts["browser.runtime.target"] != identity["target"]:
        fail(
            "browser_runtime_target_mismatch",
            "browser runtime target must match the enclosing component identity",
            expected=identity["target"],
            actual=contracts["browser.runtime.target"],
        )

    for key in [
        "browser.node.version",
        "browser.playwright.version",
        "browser.chromium.revision",
        "browser.protocol.version",
    ]:
        validate_token(key, contracts[key])

    runtime_root = normalized_relative(
        contracts["browser.runtime.root"],
        "contracts.browser.runtime.root",
    )
    path_keys = [
        "browser.node.path",
        "browser.playwright.module-path",
        "browser.playwright.browsers-path",
        "browser.chromium.executable-path",
        "browser.license.node-path",
        "browser.license.playwright-path",
        "browser.license.chromium-path",
        "browser.symlink-manifest.path",
    ]
    paths = {
        key: normalized_relative(contracts[key], f"contracts.{key}")
        for key in path_keys
    }
    for key, path in paths.items():
        if not path_is_or_descendant(path, runtime_root):
            fail(
                "browser_runtime_path_escape",
                "browser runtime capability path is outside its fixed root",
                field=key,
                path=path,
                root=runtime_root,
            )
    browsers_root = paths["browser.playwright.browsers-path"]
    chromium_path = paths["browser.chromium.executable-path"]
    if not path_is_or_descendant(chromium_path, browsers_root) or chromium_path == browsers_root:
        fail(
            "browser_runtime_chromium_outside_store",
            "Chromium executable must be below the fixed Playwright browser store",
            chromium_path=chromium_path,
            browsers_root=browsers_root,
        )

    records = {record["path"]: record for record in files}
    required_files = {
        paths["browser.node.path"],
        paths["browser.playwright.module-path"],
        chromium_path,
        paths["browser.license.node-path"],
        paths["browser.license.playwright-path"],
        paths["browser.license.chromium-path"],
        paths["browser.symlink-manifest.path"],
    }
    if provenance_present:
        source_manifest_path = normalized_relative(
            contracts["browser.component.source-manifest-path"],
            "contracts.browser.component.source-manifest-path",
        )
        required_files.add(source_manifest_path)
        source_manifest_id = contracts["browser.component.source-manifest-id"]
        if not source_manifest_id.startswith("sha256:") or not SHA256_RE.fullmatch(
            source_manifest_id[7:]
        ):
            fail(
                "browser_runtime_source_manifest_id_invalid",
                "browser runtime source manifest identity is not canonical SHA-256",
            )
    missing_files = sorted(required_files - records.keys())
    if missing_files:
        fail(
            "browser_runtime_required_file_missing",
            "browser runtime contract references files absent from the exact catalog",
            missing=missing_files,
        )
    for executable_path in [paths["browser.node.path"], chromium_path]:
        if not records[executable_path]["executable"]:
            fail(
                "browser_runtime_executable_mode_missing",
                "browser runtime executable path is not executable",
                path=executable_path,
            )
    if not any(path.startswith(browsers_root + "/") for path in records):
        fail(
            "browser_runtime_store_empty",
            "Playwright browser store contains no catalogued files",
            path=browsers_root,
        )

    disk_budget_raw = contracts["browser.disk-budget.bytes"]
    if not disk_budget_raw.isascii() or not disk_budget_raw.isdigit():
        fail(
            "browser_runtime_disk_budget_invalid",
            "browser runtime disk budget must be a canonical decimal byte count",
            actual=disk_budget_raw,
        )
    disk_budget = int(disk_budget_raw)
    component_bytes = sum(
        record["bytes"]
        for record in files
        if path_is_or_descendant(record["path"], runtime_root)
    )
    if component_bytes == 0 or component_bytes > disk_budget:
        fail(
            "browser_runtime_disk_budget_exceeded",
            "browser runtime exceeds its declared installed-byte budget",
            component_bytes=component_bytes,
            disk_budget_bytes=disk_budget,
        )


def validate_browser_runtime_symlinks(
    root: AnchoredRoot,
    manifest: dict[str, Any],
    observed: dict[str, str],
) -> None:
    contracts = manifest["contracts"]
    if contracts.get("browser.runtime.schema") != BROWSER_RUNTIME_SCHEMA:
        if observed:
            fail(
                "non_regular_inventory_entry",
                "package contains symlinks outside the browser runtime contract",
                paths=sorted(observed),
            )
        return
    if len(observed) > 256:
        fail(
            "browser_runtime_symlink_limit_exceeded",
            "browser runtime exceeds the bounded symlink allowance",
            maximum=256,
            actual=len(observed),
        )
    runtime_root = normalized_relative(
        contracts["browser.runtime.root"],
        "contracts.browser.runtime.root",
    )
    sidecar_path = normalized_relative(
        contracts["browser.symlink-manifest.path"],
        "contracts.browser.symlink-manifest.path",
    )
    evidence = root.read_regular(sidecar_path)
    if evidence.payload is None:
        fail(
            "browser_runtime_symlink_manifest_oversized",
            "browser runtime symlink manifest exceeds the bounded verifier limit",
            path=sidecar_path,
        )
    sidecar = load_json_strict(evidence.payload, sidecar_path)
    require_exact_keys(sidecar, {"links", "schema_version"}, "browser symlink manifest")
    if sidecar["schema_version"] != "ft.browser_runtime_symlinks.v1":
        fail(
            "browser_runtime_symlink_schema_mismatch",
            "browser runtime symlink manifest schema is unsupported",
            actual=sidecar["schema_version"],
        )
    links = sidecar["links"]
    if not isinstance(links, list) or len(links) > 256:
        fail(
            "browser_runtime_symlink_manifest_invalid",
            "browser runtime symlink manifest links must be a bounded array",
        )
    declared: dict[str, str] = {}
    previous = ""
    for index, record in enumerate(links):
        if not isinstance(record, dict):
            fail("browser_runtime_symlink_manifest_invalid", "browser symlink record must be an object")
        require_exact_keys(record, {"path", "target"}, f"browser symlink record {index}")
        path = normalized_relative(record["path"], f"browser symlink record {index}.path")
        target = record["target"]
        if (
            not isinstance(target, str)
            or not target
            or "\\" in target
            or "\x00" in target
            or len(target.encode("utf-8")) > MAX_PATH_BYTES
            or PurePosixPath(target).is_absolute()
        ):
            fail(
                "browser_runtime_symlink_target_invalid",
                "browser runtime symlink target must be a bounded relative POSIX path",
                path=path,
            )
        if path <= previous or path in declared:
            fail(
                "browser_runtime_symlink_order_invalid",
                "browser runtime symlink records must be unique and strictly path-sorted",
                path=path,
            )
        package_path = f"{runtime_root}/{path}"
        declared[package_path] = target
        previous = path
    if declared != observed:
        fail(
            "browser_runtime_symlink_manifest_mismatch",
            "browser runtime symlink inventory differs from its exact sidecar",
            missing=sorted(set(observed) - set(declared)),
            uncatalogued=sorted(set(declared) - set(observed)),
            retargeted=sorted(
                path
                for path in set(declared) & set(observed)
                if declared[path] != observed[path]
            ),
        )
    runtime_absolute = (root.path / runtime_root).resolve(strict=True)
    for path in declared:
        try:
            resolved = (root.path / path).resolve(strict=True)
            if resolved == runtime_absolute:
                raise ValueError("symlink resolves to the browser runtime root")
            resolved.relative_to(runtime_absolute)
        except (OSError, RuntimeError, ValueError) as exc:
            fail(
                "browser_runtime_symlink_target_escape",
                f"browser runtime symlink does not resolve within its fixed root: {exc}",
                path=path,
            )


def verify_browser_runtime_quarantine(root: Path, manifest: dict[str, Any]) -> None:
    contracts = manifest["contracts"]
    if contracts.get("browser.runtime.schema") != BROWSER_RUNTIME_SCHEMA:
        return
    absent_codes = {errno.ENODATA, errno.ENOTSUP}
    enoattr = getattr(errno, "ENOATTR", None)
    if enoattr is not None:
        absent_codes.add(enoattr)
    for key in ["browser.node.path", "browser.chromium.executable-path"]:
        relative = normalized_relative(contracts[key], f"contracts.{key}")
        try:
            path = root / relative
            if hasattr(os, "getxattr"):
                os.getxattr(path, "com.apple.quarantine", follow_symlinks=False)
            elif sys.platform == "darwin":
                import ctypes

                libc = ctypes.CDLL(None, use_errno=True)
                getxattr = libc.getxattr
                getxattr.argtypes = [
                    ctypes.c_char_p,
                    ctypes.c_char_p,
                    ctypes.c_void_p,
                    ctypes.c_size_t,
                    ctypes.c_uint32,
                    ctypes.c_int,
                ]
                getxattr.restype = ctypes.c_ssize_t
                result = getxattr(
                    os.fsencode(path),
                    b"com.apple.quarantine",
                    None,
                    0,
                    0,
                    0,
                )
                if result < 0:
                    raise OSError(ctypes.get_errno(), "getxattr failed")
            else:
                raise OSError(errno.ENOTSUP, "extended attributes are unsupported")
        except OSError as exc:
            if exc.errno in absent_codes:
                continue
            fail(
                "browser_runtime_quarantine_check_failed",
                "browser runtime quarantine state could not be inspected",
                path=relative,
                error_kind=errno.errorcode.get(exc.errno, "unknown"),
            )
        fail(
            "browser_runtime_quarantined",
            "browser runtime executable carries Gatekeeper quarantine metadata",
            path=relative,
            remedy="obtain a release-installed component without quarantine metadata",
        )


def verify_browser_runtime_provenance(root: AnchoredRoot, manifest: dict[str, Any]) -> None:
    contracts = manifest["contracts"]
    if "browser.component.source-manifest-id" not in contracts:
        return
    relative = normalized_relative(
        contracts["browser.component.source-manifest-path"],
        "contracts.browser.component.source-manifest-path",
    )
    evidence = root.read_regular(relative)
    if evidence.payload is None:
        fail(
            "browser_runtime_source_manifest_oversized",
            "browser runtime source manifest exceeds the bounded verifier limit",
            path=relative,
        )
    source_manifest = load_json_strict(evidence.payload, relative)
    claimed = source_manifest.get("manifest_id")
    if not isinstance(claimed, str):
        fail(
            "browser_runtime_source_manifest_id_missing",
            "browser runtime source manifest has no canonical manifest identity",
            path=relative,
        )
    authority = contracts["browser.component.source-manifest-id"]
    actual = "sha256:" + sha256_bytes(canonical_bytes(manifest_without_id(source_manifest)))
    if claimed != authority or actual != authority:
        fail(
            "browser_runtime_source_manifest_id_mismatch",
            "embedded browser runtime source manifest does not match its provenance contract",
            path=relative,
            expected=authority,
            claimed=claimed,
            actual=actual,
        )


def verify_manifest_shape(manifest: dict[str, Any]) -> tuple[dict[str, str], list[dict[str, Any]]]:
    require_exact_keys(manifest, TOP_LEVEL_KEYS, "manifest")
    if manifest["$schema"] != SCHEMA_URI or manifest["schema_version"] != SCHEMA_VERSION:
        fail(
            "schema_version_mismatch",
            "manifest schema identity is unsupported",
            expected={"$schema": SCHEMA_URI, "schema_version": SCHEMA_VERSION},
            actual={"$schema": manifest["$schema"], "schema_version": manifest["schema_version"]},
            remedy="use the verifier shipped with this manifest schema",
        )
    manifest_id = manifest["manifest_id"]
    if (
        not isinstance(manifest_id, str)
        or not manifest_id.startswith("sha256:")
        or not SHA256_RE.fullmatch(manifest_id[7:])
    ):
        fail("invalid_manifest_id", "manifest_id must be sha256 followed by 64 lowercase hex characters")
    expected_id = "sha256:" + sha256_bytes(canonical_bytes(manifest_without_id(manifest)))
    if manifest_id != expected_id:
        fail(
            "manifest_id_mismatch",
            "manifest canonical content hash does not match manifest_id",
            expected=expected_id,
            actual=manifest_id,
            remedy="obtain the intact manifest emitted with this exact package",
        )
    identity = validate_identity_mapping(manifest["identity"])

    contracts = manifest["contracts"]
    if not isinstance(contracts, dict) or not all(
        isinstance(key, str)
        and len(key.encode("utf-8")) <= MAX_PATH_BYTES
        and isinstance(value, str)
        and len(value.encode("utf-8")) <= MAX_PATH_BYTES
        and not any(ord(char) < 0x20 for char in value)
        and ROLE_RE.fullmatch(key)
        for key, value in contracts.items()
    ):
        fail("invalid_contracts", "contracts must map canonical keys to string values")
    if list(contracts) != sorted(contracts):
        fail("noncanonical_contract_order", "contract keys must be lexically ordered")

    files = manifest["files"]
    if not isinstance(files, list) or not files:
        fail("invalid_file_catalog", "files must be a non-empty JSON array")
    if len(files) > MAX_FILES:
        fail("catalog_too_large", "file catalog exceeds the bounded verifier limit", maximum_files=MAX_FILES)
    seen_paths: set[str] = set()
    previous_path: str | None = None
    for index, record in enumerate(files):
        field = f"files[{index}]"
        if not isinstance(record, dict):
            fail("schema_type_mismatch", f"{field} must be an object", field=field)
        expected_keys = {"bytes", "executable", "kind", "path", "role", "sha256"}
        has_component = "component" in record
        component = record.get("component")
        if has_component:
            expected_keys.add("component")
        require_exact_keys(record, expected_keys, field)
        path = normalized_relative(record["path"], f"{field}.path") if isinstance(record["path"], str) else fail(
            "schema_type_mismatch", f"{field}.path must be a string", field=f"{field}.path"
        )
        if previous_path is not None and path <= previous_path:
            fail("noncanonical_file_order", "file records must be strictly path-sorted", path=path)
        previous_path = path
        if path in seen_paths:
            fail("duplicate_catalog_path", "file path is repeated", path=path)
        seen_paths.add(path)
        if not isinstance(record["kind"], str) or record["kind"] not in ALLOWED_KINDS:
            fail("invalid_file_kind", f"unsupported file kind {record['kind']!r}", path=path)
        if (
            not isinstance(record["role"], str)
            or len(record["role"].encode("utf-8")) > MAX_PATH_BYTES
            or not ROLE_RE.fullmatch(record["role"])
        ):
            fail("invalid_role", f"invalid role in {field}", actual=record["role"])
        validate_nonnegative_int(record["bytes"], f"{field}.bytes")
        if type(record["executable"]) is not bool:
            fail("schema_type_mismatch", f"{field}.executable must be boolean")
        if not isinstance(record["sha256"], str) or not SHA256_RE.fullmatch(record["sha256"]):
            fail("invalid_file_digest", f"{field}.sha256 is not canonical SHA-256")
        if record["kind"] == "executable" and not has_component:
            fail(
                "executable_component_required",
                f"{field} must carry a component identity",
                path=path,
            )
        if has_component:
            if record["kind"] != "executable" or not isinstance(component, str):
                fail("invalid_component_entry", f"{field}.component is invalid")
            validate_token("component", component)

    inputs = manifest["inputs"]
    if not isinstance(inputs, list):
        fail("schema_type_mismatch", "inputs must be an array")
    if len(inputs) > MAX_INPUTS:
        fail("too_many_inputs", "input count exceeds the bounded verifier limit", maximum_inputs=MAX_INPUTS)
    previous_input: str | None = None
    input_roles: set[str] = set()
    for index, record in enumerate(inputs):
        field = f"inputs[{index}]"
        if not isinstance(record, dict):
            fail("schema_type_mismatch", f"{field} must be an object")
        require_exact_keys(record, {"bytes", "path", "role", "sha256"}, field)
        if not isinstance(record["path"], str):
            fail("schema_type_mismatch", f"{field}.path must be a string")
        path = normalized_relative(record["path"], f"{field}.path")
        if previous_input is not None and path <= previous_input:
            fail("noncanonical_input_order", "input records must be strictly path-sorted", path=path)
        previous_input = path
        if (
            not isinstance(record["role"], str)
            or len(record["role"].encode("utf-8")) > MAX_PATH_BYTES
            or not ROLE_RE.fullmatch(record["role"])
        ):
            fail("invalid_role", f"invalid role in {field}")
        if record["role"] in input_roles:
            fail("duplicate_input", "input role is repeated", role=record["role"])
        input_roles.add(record["role"])
        validate_nonnegative_int(record["bytes"], f"{field}.bytes")
        if not isinstance(record["sha256"], str) or not SHA256_RE.fullmatch(record["sha256"]):
            fail("invalid_file_digest", f"{field}.sha256 is not canonical SHA-256")

    inventory = manifest["inventory"]
    if not isinstance(inventory, dict):
        fail("schema_type_mismatch", "inventory must be an object")
    require_exact_keys(inventory, {"file_count", "mode", "total_bytes"}, "inventory")
    if inventory["mode"] != "exact":
        fail("inventory_mode_mismatch", "only exact package inventory is supported")
    if validate_nonnegative_int(inventory["file_count"], "inventory.file_count") != len(files):
        fail("inventory_count_mismatch", "inventory.file_count does not equal catalog length")
    total_bytes = sum(record["bytes"] for record in files)
    if validate_nonnegative_int(inventory["total_bytes"], "inventory.total_bytes") != total_bytes:
        fail("inventory_bytes_mismatch", "inventory.total_bytes does not equal catalog byte total")

    verification = manifest["verification"]
    if not isinstance(verification, dict):
        fail("schema_type_mismatch", "verification must be an object")
    require_exact_keys(verification, {"algorithm", "offline", "path_policy"}, "verification")
    expected_path_policy = (
        BROWSER_RUNTIME_PATH_POLICY
        if contracts.get("browser.runtime.schema") == BROWSER_RUNTIME_SCHEMA
        else "relative_utf8_no_symlink_v1"
    )
    if verification != {
        "algorithm": "sha256",
        "offline": True,
        "path_policy": expected_path_policy,
    }:
        fail("verification_contract_mismatch", "unsupported verification contract", actual=verification)
    validate_browser_runtime_contract(identity, contracts, files)
    return identity, files


def verify(args: argparse.Namespace) -> dict[str, Any]:
    manifest_read = read_absolute_regular(Path(args.manifest), "manifest")
    if manifest_read.evidence.payload is None:
        fail("manifest_unreadable", "manifest bytes were not retained for parsing")
    manifest = load_json_strict(manifest_read.evidence.payload, str(manifest_read.path))
    identity, files = verify_manifest_shape(manifest)
    with AnchoredRoot(Path(args.root), "package root") as root:
        manifest_rel = output_relative_to_root(root.path, manifest_read.path)
        ignored = {manifest_rel} if manifest_rel is not None else set()
        initial = root.scan_inventory(ignored)
        if initial.invalid:
            fail(
                "non_regular_inventory_entry",
                "package contains symlink or special filesystem entries",
                paths=list(initial.invalid),
            )
        expected_files = {record["path"] for record in files}
        actual_files = set(initial.files)
        validate_browser_runtime_symlinks(root, manifest, initial.symlinks)
        if actual_files != expected_files:
            fail(
                "package_inventory_mismatch",
                "offline inventory does not exactly match the manifest",
                missing=sorted(expected_files - actual_files),
                uncatalogued=sorted(actual_files - expected_files),
                remedy="use the exact package and manifest emitted together; do not merge package directories",
            )
        for record in files:
            relative = record["path"]
            component = record.get("component")
            evidence = root.read_regular(relative, scan_markers=component is not None)
            require_inventory_identity(evidence, initial.files[relative], relative)
            if evidence.sha256 != record["sha256"] or evidence.length != record["bytes"]:
                fail(
                    "file_content_mismatch",
                    f"packaged file {relative} does not match its manifest",
                    path=relative,
                    expected_sha256=record["sha256"],
                    actual_sha256=evidence.sha256,
                    expected_bytes=record["bytes"],
                    actual_bytes=evidence.length,
                    remedy="discard the mixed or corrupt package and obtain one complete atomic build",
                )
            if evidence.executable != record["executable"]:
                fail(
                    "file_mode_mismatch",
                    f"packaged file {relative} executable mode does not match its manifest",
                    path=relative,
                    expected=record["executable"],
                    actual=evidence.executable,
                )
            if component is not None:
                verify_component_markers(evidence.markers, relative, component, identity)
        verify_browser_runtime_provenance(root, manifest)
        verify_browser_runtime_quarantine(root.path, manifest)
        final = root.scan_inventory(ignored)
        require_stable_inventory(initial, final)
        root.assert_path_identity()
    assert_absolute_read_current(manifest_read, "manifest")
    return {
        "ok": True,
        "operation": "verify",
        "manifest": str(manifest_read.path),
        "manifest_id": manifest["manifest_id"],
        "build_id": identity["build_id"],
        "file_count": len(files),
        "total_bytes": sum(record["bytes"] for record in files),
        "offline": True,
    }


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(
        prog="atomic-component-manifest.sh",
        description="Generate or verify FrankenTerm atomic component manifests.",
    )
    sub = root.add_subparsers(dest="command", required=True)

    derive = sub.add_parser("derive-build-id", help="derive the canonical shared build identity")
    for option in ["source-revision", "version", "target", "profile", "feature-contract"]:
        derive.add_argument(f"--{option}", required=True)

    generate_parser = sub.add_parser("generate", help="generate a manifest from a complete staging root")
    generate_parser.add_argument("--root", required=True)
    generate_parser.add_argument("--source-root", required=True)
    generate_parser.add_argument("--output", required=True)
    generate_parser.add_argument("--build-id", required=True)
    generate_parser.add_argument("--source-revision", required=True)
    generate_parser.add_argument("--version", required=True)
    generate_parser.add_argument("--target", required=True)
    generate_parser.add_argument("--profile", required=True)
    generate_parser.add_argument("--feature-contract", required=True)
    generate_parser.add_argument(
        "--entry",
        action="append",
        default=[],
        metavar="KIND:ROLE:PATH[:COMPONENT]",
        help="catalog one required file; COMPONENT enforces the embedded build marker",
    )
    generate_parser.add_argument(
        "--tree",
        action="append",
        default=[],
        metavar="KIND:ROLE_PREFIX:PATH",
        help="catalog every regular file below one required directory",
    )
    generate_parser.add_argument(
        "--optional-tree",
        action="append",
        default=[],
        metavar="KIND:ROLE_PREFIX:PATH",
        help="catalog a directory when present",
    )
    generate_parser.add_argument("--source-match", action="append", default=[], metavar="PACKAGE_PATH=SOURCE_PATH")
    generate_parser.add_argument("--input", action="append", default=[], metavar="ROLE=SOURCE_PATH")
    generate_parser.add_argument("--contract", action="append", default=[], metavar="KEY=VALUE")

    verify_parser = sub.add_parser("verify", help="verify a package and manifest without network access")
    verify_parser.add_argument("--root", required=True)
    verify_parser.add_argument("--manifest", required=True)
    return root


def main() -> int:
    args = parser().parse_args()
    try:
        if args.command == "derive-build-id":
            result: Any = derive_build_identity(
                args.source_revision,
                args.version,
                args.target,
                args.profile,
                args.feature_contract,
            )
            print(result)
            return 0
        result = generate(args) if args.command == "generate" else verify(args)
        print(json.dumps(result, ensure_ascii=False, sort_keys=True))
        return 0
    except ManifestError as exc:
        payload = {
            "ok": False,
            "error": {
                "code": exc.code,
                "message": exc.message,
                **exc.details,
            },
        }
        print(json.dumps(payload, ensure_ascii=False, sort_keys=True), file=sys.stderr)
        return 1
    except (UnicodeError, RecursionError) as exc:
        payload = {
            "ok": False,
            "error": {
                "code": "invalid_text_encoding_or_nesting",
                "message": f"input exceeds canonical UTF-8 or nesting constraints: {exc}",
            },
        }
        print(json.dumps(payload, ensure_ascii=True, sort_keys=True), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
PY
