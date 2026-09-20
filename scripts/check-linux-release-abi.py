#!/usr/bin/env python3
"""Check direct ELF ABI needs against captured, architecture-specific providers.

This is a packaging refusal gate, not proof of successful dynamic loading,
CPU compatibility, symbol presence, or application startup on the baseline OS.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import unittest


class Refusal(ValueError):
    pass


def require(condition, message):
    if not condition:
        raise Refusal(message)


def field(text, name):
    matches = re.findall(r"^\s*" + re.escape(name) + r":\s*(.*?)\s*$", text, re.M)
    require(len(matches) == 1, "missing or duplicate ELF field: " + name)
    return matches[0]


def version_needs(text):
    """Parse only verneed, requiring each advertised file/auxiliary count."""
    section = re.findall(
        r"^Version needs section .*? contains (\d+) entr(?:y|ies):\n(.*?)(?=^Version .* section|\Z)",
        text, re.M | re.S,
    )
    require(len(section) == 1, "missing or duplicate version needs section")
    expected_files, body = section[0]
    providers = []
    current = None
    for line in body.splitlines():
        if not line.strip() or line.strip().startswith("Addr:"):
            continue
        file_match = re.fullmatch(
            r"\s*(?:0x)?[0-9a-fA-F]+:\s+Version: 1\s+File: (\S+)\s+Cnt: (\d+)\s*", line
        )
        if file_match:
            current = [file_match[1], int(file_match[2]), []]
            providers.append(current)
            continue
        version = re.fullmatch(
            r"\s*(?:0x)?[0-9a-fA-F]+:\s+Name: (\S+)\s+Flags: (none|WEAK)\s+Version: (\d+)\s*", line
        )
        require(version is not None and current is not None, "unparsed version needs entry")
        current[2].append((version[1], version[2] == "WEAK"))
    require(len(providers) == int(expected_files) and providers, "version provider count mismatch")
    require(len({p[0] for p in providers}) == len(providers), "duplicate version provider")
    for _, count, versions in providers:
        require(count > 0 and count == len(versions), "version auxiliary count mismatch")
    return providers


def supported_name(name):
    # Base SONAME definitions are not symbol versions. Non-numeric versions
    # require both this explicit category and exact provider evidence below.
    return bool(re.fullmatch(r"(?:GLIBC|GLIBCXX|CXXABI|GCC|ZLIB)_\d+(?:\.\d+)+", name)) or name in {
        "CXXABI_FLOAT128", "CXXABI_TM_1",
    }


def check_reports(header, program, dynamic, versions, policy):
    require(field(header, "Class") == "ELF64", "ELF class mismatch")
    require(field(header, "Data") == "2's complement, little endian", "ELF byte order mismatch")
    require(field(header, "Machine") == policy["machine"], "ELF machine mismatch")
    require(field(header, "Type").split(" ", 1)[0] in {"DYN", "EXEC"}, "not an executable ELF type")
    interpreters = re.findall(r"\[Requesting program interpreter: ([^\]]+)\]", program)
    require(interpreters == [policy["interpreter"]], "ELF interpreter mismatch")
    dynamic_sections = re.findall(r"^Dynamic section .* contains (\d+) entries:\n(.*)\Z",
                                  dynamic, re.M | re.S)
    require(len(dynamic_sections) == 1, "missing dynamic section")
    count, body = dynamic_sections[0]
    entries = [line for line in body.splitlines() if line.strip() and not line.strip().startswith("Tag ")]
    require(len(entries) == int(count) and all(
        re.fullmatch(r"\s*0x[0-9a-fA-F]+\s+\([^\)]+\)\s+.+", line) for line in entries
    ), "unparsed or truncated dynamic section")
    require(re.search(r"\(NULL\)\s+0x0\s*$", dynamic, re.M) is not None, "unterminated dynamic section")
    require(not re.search(r"\((?:RPATH|RUNPATH|FILTER|AUXILIARY|AUDIT|DEPAUDIT)\)", dynamic),
            "unqualified dynamic library search or substitution")
    needed = re.findall(r"\(NEEDED\)\s+Shared library: \[([^\]]+)\]", dynamic)
    require(needed and len(needed) == len(re.findall(r"\(NEEDED\)", dynamic)),
            "missing or unparsed DT_NEEDED")
    require(len(needed) == len(set(needed)), "duplicate DT_NEEDED")
    providers = policy["providers"]
    for name in needed:
        require(name in providers, "unknown dependency provider: " + name)
    needs = version_needs(versions)
    verneed_count = re.findall(r"\(VERNEEDNUM\)\s+(\d+)\s*$", dynamic, re.M)
    require(verneed_count == [str(len(needs))], "dynamic version provider count mismatch")
    require(len(re.findall(r"\(VERNEED\)", dynamic)) == 1, "missing version needs address")
    strong = []
    for provider, _, entries in needs:
        require(provider in needed and provider in providers, "unknown version provider: " + provider)
        defined = providers[provider]["defined_versions"]
        for version, weak in entries:
            require(version != "GLIBC_PRIVATE", "private glibc dependency")
            require(supported_name(version), "unsupported version namespace: " + version)
            if not weak:
                require(version in defined, "unsupported provider version: " + provider + ":" + version)
                strong.append({"provider": provider, "version": version})
    require(strong, "no strong version requirements")
    return {"needed": needed, "strong_versions": strong}


def readelf(tool, option, binary):
    result = subprocess.run(
        [tool, "--wide", option, str(binary)], capture_output=True, text=True,
        env={**os.environ, "LC_ALL": "C"}, timeout=30, check=False,
    )
    require(result.returncode == 0 and not result.stderr.strip(), "readelf failed or warned: " + option)
    return result.stdout


def verify(binary, policy, tool):
    before = hashlib.sha256(binary.read_bytes()).hexdigest()
    reports = [readelf(tool, option, binary) for option in
               ("--file-header", "--program-headers", "--dynamic", "--version-info")]
    result = check_reports(*reports, policy)
    require(hashlib.sha256(binary.read_bytes()).hexdigest() == before, "binary changed during verification")
    return {"path": str(binary), "sha256": before, **result}


class AbiParserTests(unittest.TestCase):
    """Small hostile readelf transcripts exercise the refusal boundary."""

    def setUp(self):
        self.policy = json.loads((Path(__file__).resolve().parent.parent /
                                  "release/linux-abi-baseline.json").read_text())["targets"][
                                      "x86_64-unknown-linux-gnu"]
        self.reports = [
            "Class: ELF64\nData: 2's complement, little endian\n"
            "Type: DYN (Position-Independent Executable file)\nMachine: Advanced Micro Devices X86-64\n",
            "[Requesting program interpreter: /lib64/ld-linux-x86-64.so.2]\n",
            "Dynamic section at offset 0x100 contains 4 entries:\n"
            "0x1 (NEEDED) Shared library: [libc.so.6]\n"
            "0x2 (VERNEED) 0x123\n0x3 (VERNEEDNUM) 1\n0x0 (NULL) 0x0\n",
            "Version needs section '.gnu.version_r' contains 1 entry:\n"
            " Addr: 0x10 Offset: 0x10 Link: 1 (.dynstr)\n"
            " 000000: Version: 1 File: libc.so.6 Cnt: 1\n"
            " 0x0010: Name: GLIBC_2.35 Flags: none Version: 2\n",
        ]

    def test_supported_provider(self):
        self.assertEqual(check_reports(*self.reports, self.policy)["strong_versions"],
                         [{"provider": "libc.so.6", "version": "GLIBC_2.35"}])

    def test_refusals(self):
        cases = [
            (0, "ELF64", "ELF32"),
            (0, "Advanced Micro Devices X86-64", "AArch64"),
            (1, "ld-linux-x86-64.so.2", "ld-linux-aarch64.so.1"),
            (2, "libc.so.6", "libunknown.so.1"),
            (2, "(NULL)", "(RUNPATH)"),
            (2, "(VERNEEDNUM) 1", "(VERNEEDNUM) 2"),
            (2, "contains 4 entries", "contains 5 entries"),
            (3, "GLIBC_2.35", "GLIBC_2.43"),
            (3, "GLIBC_2.35", "GLIBC_PRIVATE"),
            (3, "GLIBC_2.35", "GLIBC_ABI_DT_RELR"),
            (3, "GLIBC_2.35", "libc.so.6"),
            (3, "GLIBC_2.35", "GCC_3.0"),  # present elsewhere, wrong provider
            (3, "Cnt: 1", "Cnt: 2"),
            (3, "Flags: none", "Flags: UNKNOWN"),
            (3, "Version needs section", "Version definition section"),
        ]
        for index, before, after in cases:
            with self.subTest(after=after):
                reports = self.reports.copy()
                reports[index] = reports[index].replace(before, after)
                with self.assertRaises(Refusal):
                    check_reports(*reports, self.policy)

    def test_missing_reports_refused(self):
        for index in range(4):
            with self.subTest(index=index):
                reports = self.reports.copy()
                reports[index] = ""
                with self.assertRaises(Refusal):
                    check_reports(*reports, self.policy)

    def test_definitions_cannot_authorize_needs(self):
        self.reports[3] = ("Version definition section '.gnu.version_d' contains 1 entry:\n"
                           "  Name: GLIBC_2.43\n") + self.reports[3].replace("GLIBC_2.35", "GLIBC_2.43")
        with self.assertRaises(Refusal):
            check_reports(*self.reports, self.policy)

    def test_weak_optional_version(self):
        self.reports[3] = self.reports[3].replace("Cnt: 1", "Cnt: 2") + (
            " 0x0020: Name: GLIBC_2.39 Flags: WEAK Version: 3\n")
        self.assertEqual(len(check_reports(*self.reports, self.policy)["strong_versions"]), 1)

    def test_cpp_floor_and_explicit_nonnumeric(self):
        self.reports[2] = self.reports[2].replace("libc.so.6", "libstdc++.so.6")
        self.reports[3] = self.reports[3].replace("libc.so.6", "libstdc++.so.6")
        for version, allowed in [("GLIBCXX_3.4.30", True), ("GLIBCXX_3.4.31", False),
                                 ("CXXABI_1.3.13", True), ("CXXABI_1.3.14", False),
                                 ("CXXABI_TM_1", True), ("CXXABI_FAKE", False)]:
            with self.subTest(version=version):
                reports = self.reports.copy()
                reports[3] = reports[3].replace("GLIBC_2.35", version)
                if allowed:
                    check_reports(*reports, self.policy)
                else:
                    with self.assertRaises(Refusal):
                        check_reports(*reports, self.policy)

    def test_arm_uses_arm_provider_definitions(self):
        self.policy = json.loads((Path(__file__).resolve().parent.parent /
                                  "release/linux-abi-baseline.json").read_text())["targets"][
                                      "aarch64-unknown-linux-gnu"]
        self.reports[0] = self.reports[0].replace("Advanced Micro Devices X86-64", "AArch64")
        self.reports[1] = "[Requesting program interpreter: /lib/ld-linux-aarch64.so.1]\n"
        check_reports(*self.reports, self.policy)
        # Numeric version is older but absent from this architecture's libc.
        self.reports[3] = self.reports[3].replace("GLIBC_2.35", "GLIBC_2.2.5")
        with self.assertRaises(Refusal):
            check_reports(*self.reports, self.policy)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    parser.add_argument("--baseline", type=Path,
                        default=Path(__file__).resolve().parent.parent / "release/linux-abi-baseline.json")
    parser.add_argument("--ft", type=Path, required=True)
    parser.add_argument("--mux", type=Path, required=True)
    parser.add_argument("--guardian", type=Path, required=True)
    args = parser.parse_args()
    receipt = {"schema": "frankenterm-linux-abi-check-v1", "target": args.target,
               "status": "failed", "components": {}}
    try:
        baseline_bytes = args.baseline.read_bytes()
        baseline = json.loads(baseline_bytes)
        require(baseline["schema"] == "frankenterm-linux-abi-baseline-v1", "unknown baseline schema")
        require(args.target in baseline["targets"], "unsupported target")
        policy = baseline["targets"][args.target]
        require(policy["image"]["config_matches_pulled_image"] is True, "unbound baseline image")
        for provider in policy["providers"].values():
            require(re.fullmatch(r"[0-9a-f]{64}", provider["sha256"]), "invalid provider hash")
        tool = shutil.which("readelf")
        require(tool is not None, "readelf unavailable")
        receipt.update(baseline_sha256=hashlib.sha256(baseline_bytes).hexdigest(), image=policy["image"])
        failures = False
        for name, binary, expected in (("ft", args.ft, "ft"),
                                       ("mux", args.mux, "frankenterm-mux-server"),
                                       ("guardian", args.guardian, "frankenterm-pty-guardian")):
            identity = {"path": str(binary)}
            try:
                identity["sha256"] = hashlib.sha256(binary.read_bytes()).hexdigest()
                require(binary.name == expected, "incorrect component basename")
                receipt["components"][name] = {"status": "passed", **verify(binary, policy, tool)}
            except (Refusal, OSError, subprocess.SubprocessError) as error:
                failures = True
                receipt["components"][name] = {"status": "failed", **identity, "reason": str(error)}
        receipt["status"] = "failed" if failures else "passed"
    except (Refusal, OSError, KeyError, TypeError, ValueError, subprocess.SubprocessError) as error:
        receipt["reason"] = str(error)
    print(json.dumps(receipt, indent=2, sort_keys=True))
    return 0 if receipt["status"] == "passed" else 1


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        unittest.main(argv=[sys.argv[0]], verbosity=2)
    else:
        sys.exit(main())
