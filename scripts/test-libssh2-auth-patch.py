#!/usr/bin/env python3
"""Exercise the RSA signature-upgrade helpers and our public-key policy after patching.

Usage: python3 scripts/test-libssh2-auth-patch.py UPSTREAM_LIBSSH2_SOURCE

The input must be the exact libssh2-sys snapshot accepted by the production
preparer. No download, registry edit or full native build occurs. The small C
harness checks request/wrapper names, the SHA-1 default, allocation failures,
peer duplicates and the exact request policy; it is a diagnostic, not
Apple-native, SSH integration or RCH proof. Generated source and executable are
retained in a fresh temporary directory for review.
"""

import argparse
import os
from pathlib import Path
import shlex
import subprocess
import tempfile


def extract(source: str, first: str, last: str, what: str) -> str:
    """Return source[first, last), where each marker occurs exactly once."""
    if source.count(first) != 1 or source.count(last) != 1:
        raise RuntimeError(f"{what} boundaries changed; review the extraction")
    start, end = source.index(first), source.index(last)
    if end <= start:
        raise RuntimeError(f"invalid {what} extent")
    return source[start:end]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("upstream_source", type=Path)
    parser.add_argument("--cc", default=os.environ.get("CC", "cc"))
    args = parser.parse_args()
    scripts = Path(__file__).resolve().parent
    work = Path(tempfile.mkdtemp(prefix="ft-libssh2-auth-patch-"))
    prepared = subprocess.run(
        ["bash", str(scripts / "prepare-libssh2-auth-source.sh"),
         str(args.upstream_source.resolve(strict=True)), str(work / "sources")],
        check=True, text=True, stdout=subprocess.PIPE,
    )
    prepared_root = Path(prepared.stdout.strip())
    misc = (prepared_root / "src/misc.c").read_text()
    number_definition = "int ssh2_str_number(const char **linep,"
    number = extract(
        misc,
        "/* given an ASCII character and max ascii, return TRUE if valid */",
        number_definition,
        "version-number parser",
    )
    body_start = misc.index(number_definition)
    body_end = misc.index("\n}\n", body_start) + len("\n}\n")
    number += misc[body_start:body_end]
    userauth = (prepared_root / "src/userauth.c").read_text()
    helpers = extract(
        userauth,
        "void ssh2_userauth_plain_method(char *method)\n{",
        "/* This additive API deliberately preserves all upstream userauth return codes.",
        "signature-upgrade helper",
    )
    policy = extract(
        userauth,
        "/* Exact request names are distinct from key-blob and signature-wrapper names.",
        "int ssh2_userauth_publickey(\n",
        "public-key policy helper",
    )
    (work / "libssh2-algorithms-under-test.h").write_text(
        number + "\n" + helpers + "\n" + policy
    )
    compiler = shlex.split(args.cc)
    if not compiler:
        raise RuntimeError("a C compiler is required")
    executable = work / "key-sign-algorithms"
    subprocess.run(
        compiler + ["-std=c99", "-Wall", "-Wextra", "-Werror", "-pedantic",
                    "-I", str(work), "-I", str(prepared_root / "include"),
                    str(scripts / "fixtures/libssh2-key-sign-algorithms.c"),
                    "-o", str(executable)],
        check=True,
    )
    subprocess.run([str(executable)], check=True)
    print(f"Diagnostic artifacts retained at {work}")


if __name__ == "__main__":
    main()
