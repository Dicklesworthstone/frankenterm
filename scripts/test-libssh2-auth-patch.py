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


def verify_sftp_bounds(prepared: Path, work: Path, compiler: list[str]) -> None:
    """Compile the actual upstream response parser with ASan/UBSan."""
    misc = (prepared / "src/misc.c").read_text()
    helpers = []
    for name in ("_libssh2_check_length", "_libssh2_get_byte",
                 "_libssh2_get_u32", "_libssh2_get_string"):
        start = misc.index("int " + name + "(")
        end = misc.index("\n}", start) + 2
        helpers.append(misc[start:end])
    sftp = (prepared / "src/sftp.c").read_text()
    start = sftp.index("    buf.data = data;", sftp.index("sftp_symlink("))
    end = sftp.index("    return retcode;\n}", start)
    parser = sftp[start:end] + "    return retcode;\n}"
    harness = r'''
#include <assert.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
struct string_buf { unsigned char *data, *dataptr; size_t len; };
#define _libssh2_ntohu32(p) (((uint32_t)(p)[0]<<24)|((uint32_t)(p)[1]<<16)|((uint32_t)(p)[2]<<8)|(p)[3])
#define LIBSSH2_FREE(session, data) free(data)
#define _libssh2_error(session, code, message) (code)
#define LIBSSH2_ERROR_SFTP_PROTOCOL (-31)
#define LIBSSH2_ERROR_BUFFER_TOO_SMALL (-38)
#define LIBSSH2_ERROR_NONE 0
#define LIBSSH2_FX_OK 0
#define SSH_FXP_STATUS 101
'''
    harness += "\n".join(helpers)
    harness += r'''
static int parse(unsigned char *data, size_t data_len, char *target, size_t target_len) {
    struct string_buf buf;
    unsigned char packet_type, *lk_target;
    size_t lk_len;
    uint32_t tmp_u32;
    int retcode;
    struct { int last_errno; } state = {0}, *sftp = &state;
'''
    harness += parser
    harness += r'''
int main(void) {
    const unsigned char good[] = {104,0,0,0,1,0,0,0,1,0,0,0,3,'a','b','c'};
    unsigned cases = 0;
    for(size_t n = 0; n < sizeof(good); ++n) {
        unsigned char *packet = malloc(n ? n : 1);
        memcpy(packet, good, n);
        char out[8]; memset(out, 'X', sizeof(out));
        assert(parse(packet, n, out, sizeof(out)) == LIBSSH2_ERROR_SFTP_PROTOCOL);
        for(size_t i = 0; i < sizeof(out); ++i) assert(out[i] == 'X');
        ++cases;
    }
    for(size_t capacity = 0; capacity <= 4; ++capacity) {
        unsigned char *packet = malloc(sizeof(good)); memcpy(packet, good, sizeof(good));
        char out[8]; memset(out, 'X', sizeof(out));
        int rc = parse(packet, sizeof(good), out, capacity);
        assert(rc == (capacity > 3 ? 3 : LIBSSH2_ERROR_BUFFER_TOO_SMALL));
        if(capacity > 3) assert(strcmp(out, "abc") == 0);
        else for(size_t i = 0; i < sizeof(out); ++i) assert(out[i] == 'X');
        ++cases;
    }
    unsigned char *packet = malloc(sizeof(good)); memcpy(packet, good, sizeof(good));
    memset(packet + 9, 255, 4);
    char out[8]; memset(out, 'X', sizeof(out));
    assert(parse(packet, sizeof(good), out, sizeof(out)) == LIBSSH2_ERROR_SFTP_PROTOCOL);
    for(size_t i = 0; i < sizeof(out); ++i) assert(out[i] == 'X');
    printf("Actual SFTP response parser: %u cases passed with sanitizers\n", cases + 1);
    return 0;
}
'''
    source = work / "sftp-bounds.c"
    source.write_text(harness)
    executable = work / "sftp-bounds"
    subprocess.run(compiler + ["-std=c99", "-Wall", "-Wextra", "-Werror",  # ubs:ignore — explicit operator-selected compiler argv, never a shell command
                              "-fsanitize=address,undefined", str(source),
                              "-o", str(executable)], check=True, timeout=60)
    subprocess.run([str(executable)], check=True, timeout=30)


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
        check=True, text=True, stdout=subprocess.PIPE, timeout=60,
    )
    source = (Path(prepared.stdout.strip()) / "src/userauth.c").read_text()
    first = "size_t plain_method(char *method, size_t method_len)"
    last = "/* This additive API deliberately preserves all upstream userauth return codes."
    if source.count(first) != 1 or source.count(last) != 1:
        raise RuntimeError("patched helper boundaries changed; review the extraction")
    start, end = source.index(first), source.index(last)
    if end <= start:
        raise RuntimeError("invalid patched helper extent")
    policy_first = "/* Exact request names are distinct from key-blob and signature-wrapper names."
    policy_last = "int\n_libssh2_userauth_publickey("
    if source.count(policy_first) != 1 or source.count(policy_last) != 1:
        raise RuntimeError("public-key policy helper boundaries changed; review the extraction")
    policy_start, policy_end = source.index(policy_first), source.index(policy_last)
    if policy_end <= policy_start:
        raise RuntimeError("invalid public-key policy helper extent")
    (work / "libssh2-algorithms-under-test.h").write_text(
        source[start:end] + "\n" + source[policy_start:policy_end]
    )
    compiler = shlex.split(args.cc)
    if not compiler:
        raise RuntimeError("a C compiler is required")
    executable = work / "key-sign-algorithms"
    subprocess.run(  # ubs:ignore — explicit operator-selected compiler argv, never a shell command
        compiler + ["-std=c99", "-Wall", "-Wextra", "-Werror", "-pedantic",
                    "-I", str(work), str(scripts / "fixtures/libssh2-key-sign-algorithms.c"),
                    "-o", str(executable)],
        check=True, timeout=60,
    )
    subprocess.run([str(executable)], check=True, timeout=30)
    verify_sftp_bounds(Path(prepared.stdout.strip()), work, compiler)
    print(f"Diagnostic artifacts retained at {work}")


if __name__ == "__main__":
    main()
