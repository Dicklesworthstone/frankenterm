#!/usr/bin/env bash
# tests/e2e/test_ft_mgy5t2_33_release_channel_split.sh
# Companion adversarial verification suite for ft-mgy5t2.32 / ft-mgy5t2.33 (RC4.G94a).
# Verifies release-channel separation (stable vs companion-appstore), Linux
# GLIBC_2.35/GLIBCXX_3.4.30/CXXABI_1.3.13 ABI floor gating, and install.sh
# stable-tag selection (excluding -appstore.* companion tags).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"

python3 - "$ROOT" <<'PY'
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

root = Path(sys.argv[1])
verify_sh = root / "scripts/release/verify-release.sh"
abi_sh = root / "scripts/release/check-linux-abi-floor.sh"
install_sh = root / "install.sh"

results = []


def record(case_id: str, channel: str, case_kind: str, expected_rc: int, proc: subprocess.CompletedProcess, extra: dict | None = None):
    ok = (proc.returncode == expected_rc)
    entry = {
        "case_id": case_id,
        "channel": channel,
        "case_kind": case_kind,
        "expected_exit": expected_rc,
        "actual_exit": proc.returncode,
        "verdict": "pass" if ok else "fail",
    }
    if extra:
        entry.update(extra)
    if not ok:
        entry["stdout_tail"] = proc.stdout[-400:]
        entry["stderr_tail"] = proc.stderr[-400:]
    results.append(entry)
    return ok


with tempfile.TemporaryDirectory(prefix="ft-rc4-g94a-test-") as td:
    td_path = Path(td)

    # 1. Negative: -appstore.* tag rejected on --channel stable with channel_mismatch
    p = subprocess.run(
        ["bash", str(verify_sh), "--channel", "stable", "--json", "v0.15.22-appstore.15"],
        capture_output=True, text=True,
    )
    parsed = json.loads(p.stdout) if p.stdout.strip().startswith("{") else {}
    assert parsed.get("fail_code") == "channel_mismatch", f"expected channel_mismatch, got {parsed}"
    record("neg_appstore_on_stable", "stable", "negative", 1, p, {"fail_code": parsed.get("fail_code")})

    # 2. Negative: stable semver tag rejected on --channel companion-appstore with channel_mismatch
    p = subprocess.run(
        ["bash", str(verify_sh), "--channel", "companion-appstore", "--json", "v0.16.0"],
        capture_output=True, text=True,
    )
    parsed = json.loads(p.stdout) if p.stdout.strip().startswith("{") else {}
    assert parsed.get("fail_code") == "channel_mismatch", f"expected channel_mismatch, got {parsed}"
    record("neg_stable_on_companion", "companion-appstore", "negative", 1, p, {"fail_code": parsed.get("fail_code")})

    # 3. Positive: compliant companion-appstore fixture (prerelease=true + required exporter assets)
    good_companion = {
        "tag_name": "v0.15.22-appstore.16",
        "draft": False,
        "prerelease": True,
        "target_commitish": "93d33742e75b063115a7460da9904e5f0d360303",
        "assets": [
            {"name": "FrankenTerm-Exporter.dmg", "size": 9790605},
            {"name": "FrankenTerm-Exporter.dmg.sha256", "size": 91},
            {"name": "FrankenTerm-Exporter.dmg.minisig", "size": 371},
            {"name": "SHA256SUMS", "size": 716},
            {"name": "SHA256SUMS.minisig", "size": 357},
        ],
    }
    good_json = td_path / "companion_good.json"
    good_json.write_text(json.dumps(good_companion))
    p = subprocess.run(
        ["bash", str(verify_sh), "--channel", "companion-appstore", "--json",
         "v0.15.22-appstore.16", "--release-json", str(good_json)],
        capture_output=True, text=True,
    )
    parsed = json.loads(p.stdout) if p.stdout.strip().startswith("{") else {}
    assert parsed.get("ok") is True, f"expected ok=true, got {parsed}"
    record("pos_companion_prerelease_fixture", "companion-appstore", "positive", 0, p, {"ok": parsed.get("ok")})

    # 4. Negative: companion-appstore fixture with prerelease=false (hijacking GitHub Latest!)
    bad_companion = dict(good_companion, tag_name="v0.15.22-appstore.15", prerelease=False)
    bad_json = td_path / "companion_not_prerelease.json"
    bad_json.write_text(json.dumps(bad_companion))
    p = subprocess.run(
        ["bash", str(verify_sh), "--channel", "companion-appstore", "--json",
         "v0.15.22-appstore.15", "--release-json", str(bad_json)],
        capture_output=True, text=True,
    )
    parsed = json.loads(p.stdout) if p.stdout.strip().startswith("{") else {}
    assert parsed.get("fail_code") == "companion_release_not_prerelease", f"got {parsed}"
    record("neg_companion_hijacks_latest", "companion-appstore", "negative", 1, p, {"fail_code": parsed.get("fail_code")})

    # 5. Positive: Linux ABI floor check passes for <= GLIBC_2.35 / GLIBCXX_3.4.30 / CXXABI_1.3.13
    clean_syms = td_path / "clean_syms.txt"
    clean_syms.write_text(
        "0000000000000000  DF *UND*  0000000000000000 (GLIBC_2.17) memcpy\n"
        "0000000000000000  DF *UND*  0000000000000000 (GLIBC_2.34) pthread_create\n"
        "0000000000000000  DF *UND*  0000000000000000 (GLIBC_2.35) close_range\n"
        "0000000000000000  DF *UND*  0000000000000000 (GLIBCXX_3.4.30) _ZSt21__glibcxx_assert_fail\n"
        "0000000000000000  DF *UND*  0000000000000000 (CXXABI_1.3.13) __cxa_init_primary_exception\n"
    )
    p = subprocess.run(
        ["bash", str(abi_sh), "--json", "--symbols-file", str(clean_syms)],
        capture_output=True, text=True,
    )
    parsed = json.loads(p.stdout)
    assert parsed.get("ok") is True and parsed.get("violation_count") == 0, parsed
    record("pos_linux_abi_floor_clean", "stable", "positive", 0, p, {"max_observed": parsed.get("max_observed")})

    # 6. Negative: Linux ABI floor check rejects GLIBC_2.43 & GLIBCXX_3.4.31 (ft-gjsx6)
    bad_syms = td_path / "glibc_243_syms.txt"
    bad_syms.write_text(
        "0000000000000000  DF *UND*  0000000000000000 (GLIBC_2.35) close_range\n"
        "0000000000000000  DF *UND*  0000000000000000 (GLIBC_2.43) __isoc23_strtol\n"
        "0000000000000000  DF *UND*  0000000000000000 (GLIBCXX_3.4.31) _ZSt_future\n"
    )
    p = subprocess.run(
        ["bash", str(abi_sh), "--json", "--symbols-file", str(bad_syms)],
        capture_output=True, text=True,
    )
    parsed = json.loads(p.stdout)
    offending = {v["symbol_version"] for v in parsed.get("violations", [])}
    assert offending == {"GLIBC_2.43", "GLIBCXX_3.4.31"}, parsed
    record("neg_linux_abi_glibc_243", "stable", "negative", 1, p, {"offending": sorted(offending)})

    # 7. Positive/Negative: install.sh release_tag_with_assets skips -appstore.* companion tags & selects newest stable
    releases_fixture = [
        {
            "tag_name": "v0.15.22-appstore.15",
            "draft": False,
            "prerelease": False,
            "created_at": "2026-11-10T00:00:00Z",
            "assets": [
                {"name": "ft-darwin-arm64.tar.xz"},
                {"name": "ft-darwin-arm64.tar.xz.sha256"},
                {"name": "ft-darwin-arm64.tar.xz.minisig"},
            ],
        },
        {
            "tag_name": "v0.15.23",
            "draft": False,
            "prerelease": False,
            "created_at": "2026-11-09T00:00:00Z",
            "assets": [
                {"name": "ft-linux-amd64.tar.xz"},
                {"name": "ft-linux-amd64.tar.xz.sha256"},
                {"name": "ft-linux-amd64.tar.xz.minisig"},
                {"name": "ft-darwin-arm64.tar.xz"},
                {"name": "ft-darwin-arm64.tar.xz.sha256"},
                {"name": "ft-darwin-arm64.tar.xz.minisig"},
            ],
        },
    ]
    # Source only the function definition from install.sh without running main
    harness_cmd = (
        f"eval \"$(sed -n '/^release_tag_with_assets()/,/^}}/p' {str(install_sh)!r})\"\n"
        "release_tag_with_assets ft-darwin-arm64.tar.xz ft-darwin-arm64.tar.xz.sha256 ft-darwin-arm64.tar.xz.minisig\n"
    )
    p = subprocess.run(["bash", "-c", harness_cmd], input=json.dumps(releases_fixture), capture_output=True, text=True)
    assert p.stdout.strip() == "v0.15.23", f"expected v0.15.23, got {p.stdout!r} err={p.stderr!r}"
    record("pos_install_skips_appstore_selects_stable", "stable", "positive", 0, p, {"selected_tag": p.stdout.strip()})

    # 8. Negative: when ONLY -appstore.* exists, release_tag_with_assets refuses (exit 2)
    p = subprocess.run(
        ["bash", "-c", harness_cmd],
        input=json.dumps([releases_fixture[0]]),
        capture_output=True, text=True,
    )
    record("neg_install_refuses_appstore_only", "stable", "negative", 2, p, {"selected_tag": p.stdout.strip()})

    # 9. Syntax check on install.sh & verify-release.sh (bash -n)
    for name, path in [("install_sh_syntax", install_sh), ("verify_release_syntax", verify_sh), ("abi_floor_syntax", abi_sh)]:
        p = subprocess.run(["bash", "-n", str(path)], capture_output=True, text=True)
        record(name, "all", "positive", 0, p)

all_ok = all(r["verdict"] == "pass" for r in results)
summary = {
    "schema_version": "frankenterm.test_receipt.v1",
    "bead_id": "ft-mgy5t2.33",
    "parent_bead": "ft-mgy5t2.32",
    "ok": all_ok,
    "passed": sum(1 for r in results if r["verdict"] == "pass"),
    "failed": sum(1 for r in results if r["verdict"] != "pass"),
    "cases": results,
}
print(json.dumps(summary, indent=2))
sys.exit(0 if all_ok else 1)
PY
