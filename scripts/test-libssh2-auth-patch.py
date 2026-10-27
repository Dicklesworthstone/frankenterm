#!/usr/bin/env python3
"""Exercise the actual RSA-certificate negotiation helpers after applying our patch.

Usage: python3 scripts/test-libssh2-auth-patch.py UPSTREAM_LIBSSH2_SOURCE

The input must be the exact libssh2-sys snapshot accepted by the production
preparer. No download, registry edit or full native build occurs. The small C
harness checks request/wrapper names, allocation failures and peer duplicates;
it is a diagnostic, not Apple-native, SSH integration or RCH proof. Generated
source and executable are retained in a fresh temporary directory for review.
"""

import argparse
import os
from pathlib import Path
import shlex
import subprocess
import tempfile


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
    source = (Path(prepared.stdout.strip()) / "src/userauth.c").read_text()
    first = "size_t plain_method(char *method, size_t method_len)"
    last = "/* This additive API deliberately preserves all upstream userauth return codes."
    if source.count(first) != 1 or source.count(last) != 1:
        raise RuntimeError("patched helper boundaries changed; review the extraction")
    start, end = source.index(first), source.index(last)
    if end <= start:
        raise RuntimeError("invalid patched helper extent")
    (work / "libssh2-algorithms-under-test.h").write_text(source[start:end])
    compiler = shlex.split(args.cc)
    if not compiler:
        raise RuntimeError("a C compiler is required")
    executable = work / "key-sign-algorithms"
    subprocess.run(
        compiler + ["-std=c99", "-Wall", "-Wextra", "-Werror", "-pedantic",
                    "-I", str(work), str(scripts / "fixtures/libssh2-key-sign-algorithms.c"),
                    "-o", str(executable)],
        check=True,
    )
    subprocess.run([str(executable)], check=True)
    print(f"Diagnostic artifacts retained at {work}")


if __name__ == "__main__":
    main()
