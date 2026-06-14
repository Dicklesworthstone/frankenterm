#!/usr/bin/env python3
"""Changed-file UBS gate for FrankenTerm's large CLI file.

The global `ubs <files>` runner scans whole files. That is correct for normal
files, but `crates/frankenterm/src/main.rs` is large enough that whole-file UBS
currently replays historical panic/unwrap inventory and can stall in ast-grep.
This helper keeps the ordinary UBS path for normal files and line-gates only the
known oversized CLI file.
"""

from __future__ import annotations

import argparse
import re
import shutil
import subprocess  # nosec B404 - required for fixed-argv git/ubs subprocesses.
import sys
from dataclasses import dataclass
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
GIT_TIMEOUT_SECS = 15
UBS_TIMEOUT_SECS = 300
LINE_GATED_RUST_FILES = {
    "crates/frankenterm/src/main.rs",
}

PANIC_PATTERNS: tuple[tuple[str, re.Pattern[str]], ...] = (
    ("panic macro", re.compile(r"\bpanic!\s*\(")),
    ("unreachable macro", re.compile(r"\bunreachable!\s*\(")),
    ("todo macro", re.compile(r"\btodo!\s*\(")),
    ("unimplemented macro", re.compile(r"\bunimplemented!\s*\(")),
    ("assert macro", re.compile(r"\bassert(?:_eq|_ne)?!\s*\(")),
)

UNWRAP_PATTERNS: tuple[tuple[str, re.Pattern[str]], ...] = (
    ("unwrap call", re.compile(r"\.(?:unwrap|unwrap_err)\s*\(")),
    ("expect call", re.compile(r"\.(?:expect|expect_err)\s*\(")),
)


@dataclass(frozen=True)
class ChangedLine:
    path: str
    line_number: int
    text: str


@dataclass(frozen=True)
class Finding:
    path: str
    line_number: int
    severity: str
    kind: str
    text: str


_TEST_CONTEXT_CACHE: dict[str, set[int]] = {}


def run_git(args: list[str]) -> subprocess.CompletedProcess[str]:
    git_bin = shutil.which("git")
    if git_bin is None:
        raise RuntimeError("git not found in PATH")
    try:
        return subprocess.run(  # nosec B603 - fixed git binary, argv only.
            [git_bin, *args],
            cwd=REPO_ROOT,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=GIT_TIMEOUT_SECS,
        )
    except subprocess.TimeoutExpired as exc:
        raise RuntimeError(f"git command timed out after {GIT_TIMEOUT_SECS}s") from exc


def repo_relative(path: str) -> str:
    candidate = Path(path)
    if candidate.is_absolute():
        try:
            return candidate.resolve().relative_to(REPO_ROOT).as_posix()
        except ValueError:
            return candidate.as_posix()
    return candidate.as_posix().removeprefix("./")


def changed_files(staged: bool, explicit_files: list[str]) -> list[str]:
    if explicit_files:
        return sorted({repo_relative(path) for path in explicit_files})

    diff_args = ["diff", "--name-only", "--diff-filter=ACMR"]
    if staged:
        diff_args.append("--cached")
    else:
        diff_args.append("HEAD")
    result = run_git(diff_args)
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip() or "git diff failed")
    return sorted({line.strip() for line in result.stdout.splitlines() if line.strip()})


def iter_changed_lines(path: str, staged: bool) -> list[ChangedLine]:
    diff_args = ["diff", "--unified=0", "--no-ext-diff", "--", path]
    if staged:
        diff_args.insert(1, "--cached")
    else:
        diff_args.insert(1, "HEAD")

    result = run_git(diff_args)
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip() or f"git diff failed for {path}")

    lines: list[ChangedLine] = []
    new_line_number: int | None = None
    for raw in result.stdout.splitlines():
        if raw.startswith("@@ "):
            match = re.search(r"\+(\d+)(?:,(\d+))?", raw)
            new_line_number = int(match.group(1)) if match else None
            continue

        if new_line_number is None:
            continue

        if raw.startswith("+") and not raw.startswith("+++"):
            lines.append(ChangedLine(path=path, line_number=new_line_number, text=raw[1:]))
            new_line_number += 1
        elif raw.startswith("-") and not raw.startswith("---"):
            continue
        else:
            new_line_number += 1

    return lines


def suppression_reason(line: str) -> str | None:
    marker = "ubs:ignore"
    index = line.find(marker)
    if index < 0:
        return None
    return line[index + len(marker) :].strip(" -:\t")


def code_for_brace_count(line: str) -> str:
    line = re.sub(r'"(?:\\.|[^"\\])*"', '""', line)
    line = re.sub(r"'(?:\\.|[^'\\])'", "''", line)
    return line.split("//", 1)[0]


def brace_delta(line: str) -> int:
    code = code_for_brace_count(line)
    return code.count("{") - code.count("}")


def rust_test_context_lines(path: str) -> set[int]:
    if "/tests/" in f"/{path}" or "/benches/" in f"/{path}":
        return set(range(1, 1_000_000))
    if path in _TEST_CONTEXT_CACHE:
        return _TEST_CONTEXT_CACHE[path]

    full_path = REPO_ROOT / path
    try:
        lines = full_path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        _TEST_CONTEXT_CACHE[path] = set()
        return set()

    test_lines: set[int] = set()
    active_depths: list[int] = []
    pending_cfg_test = False
    pending_test_fn = False
    depth = 0

    for line_number, line in enumerate(lines, start=1):
        stripped = line.strip()
        opens_test_block = False
        if pending_cfg_test and re.search(r"\bmod\s+\w+\b", stripped) and "{" in stripped:
            opens_test_block = True
        if pending_test_fn and re.search(r"\bfn\s+\w+\b", stripped) and "{" in stripped:
            opens_test_block = True

        if opens_test_block:
            active_depths.append(depth)
            pending_cfg_test = False
            pending_test_fn = False

        if active_depths:
            test_lines.add(line_number)

        if stripped.startswith("#[cfg(test)]"):
            pending_cfg_test = True
        elif re.match(r"#\[(?:[A-Za-z_][A-Za-z0-9_]*::)?test\]", stripped):
            pending_test_fn = True
        elif stripped.startswith("#["):
            pass
        elif stripped and not opens_test_block:
            pending_cfg_test = False
            pending_test_fn = False

        depth += brace_delta(line)
        while active_depths and depth <= active_depths[-1]:
            active_depths.pop()

    _TEST_CONTEXT_CACHE[path] = test_lines
    return test_lines


def line_findings(line: ChangedLine) -> list[Finding]:
    stripped = line.text.strip()
    if not stripped or stripped.startswith("//"):
        return []

    reason = suppression_reason(line.text)
    if reason is not None:
        if reason:
            return []
        return [
            Finding(
                path=line.path,
                line_number=line.line_number,
                severity="critical",
                kind="unjustified ubs ignore",
                text=line.text,
            )
        ]

    findings: list[Finding] = []
    for kind, pattern in PANIC_PATTERNS:
        if pattern.search(line.text):
            findings.append(
                Finding(
                    path=line.path,
                    line_number=line.line_number,
                    severity="critical",
                    kind=kind,
                    text=line.text,
                )
            )
    for kind, pattern in UNWRAP_PATTERNS:
        if pattern.search(line.text):
            findings.append(
                Finding(
                    path=line.path,
                    line_number=line.line_number,
                    severity="warning",
                    kind=kind,
                    text=line.text,
                )
            )
    return findings


def run_line_gate(files: list[str], staged: bool) -> int:
    findings: list[Finding] = []
    for path in files:
        test_lines = rust_test_context_lines(path)
        for line in iter_changed_lines(path, staged):
            if line.line_number in test_lines:
                continue
            findings.extend(line_findings(line))

    if not findings:
        print("line-gated UBS: no panic/unwrap findings in changed CLI hunks")
        return 0

    print("line-gated UBS findings:", file=sys.stderr)
    for finding in findings:
        print(
            f"{finding.path}:{finding.line_number}: "
            f"{finding.severity}: {finding.kind}: {finding.text.strip()}",
            file=sys.stderr,
        )
    return 1


def run_ubs(files: list[str]) -> int:
    if not files:
        return 0
    ubs_bin = shutil.which("ubs")
    if ubs_bin is None:
        print("ubs not found in PATH", file=sys.stderr)
        return 2
    try:
        return subprocess.run(  # nosec B603 - fixed UBS binary, repo paths stay argv.
            [ubs_bin, *files],
            cwd=REPO_ROOT,
            check=False,
            timeout=UBS_TIMEOUT_SECS,
        ).returncode
    except subprocess.TimeoutExpired:
        print(f"ubs timed out after {UBS_TIMEOUT_SECS}s", file=sys.stderr)
        return 2


def self_test() -> int:
    safe = ChangedLine("crates/frankenterm/src/main.rs", 10, "let x = value?;")
    risky = ChangedLine("crates/frankenterm/src/main.rs", 11, "let x = value.unwrap();")
    ignored = ChangedLine(
        "crates/frankenterm/src/main.rs",
        12,
        "let x = value.unwrap(); // ubs:ignore - test fixture asserts panic",
    )
    bad_ignore = ChangedLine(
        "crates/frankenterm/src/main.rs",
        13,
        "let x = value.unwrap(); // ubs:ignore",
    )
    with_test_line = "    #[test]\n    fn flags_fixture() {\n        assert!(true);\n    }\n"
    test_path = "crates/frankenterm/src/main.rs"
    _TEST_CONTEXT_CACHE[test_path] = {
        line_number
        for line_number, text in enumerate(with_test_line.splitlines(), start=1)
        if "assert!" in text
    }
    test_line = ChangedLine(test_path, 3, "        assert!(true);")
    test_findings = (
        []
        if test_line.line_number in rust_test_context_lines(test_path)
        else line_findings(test_line)
    )
    checks = [
        (line_findings(safe), 0),
        (line_findings(risky), 1),
        (line_findings(ignored), 0),
        (line_findings(bad_ignore), 1),
        (test_findings, 0),
    ]
    failures = [
        index
        for index, (actual, expected_len) in enumerate(checks, start=1)
        if len(actual) != expected_len
    ]
    if failures:
        print(f"self-test failed: checks {failures}", file=sys.stderr)
        return 1
    print("self-test passed")
    return 0


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Run UBS on changed files, using changed-line checks for the oversized "
            "frankenterm CLI file."
        )
    )
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--staged", action="store_true", help="inspect staged hunks")
    mode.add_argument("--diff", action="store_true", help="inspect working-tree hunks")
    parser.add_argument("--self-test", action="store_true", help="run internal tests")
    parser.add_argument("files", nargs="*", help="explicit files to check")
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    if args.self_test:
        return self_test()

    staged = bool(args.staged)
    files = changed_files(staged=staged, explicit_files=args.files)
    line_gated = [path for path in files if path in LINE_GATED_RUST_FILES]
    normal = [path for path in files if path not in LINE_GATED_RUST_FILES]

    normal_status = run_ubs(normal)
    line_status = run_line_gate(line_gated, staged=staged)
    return max(normal_status, line_status)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
