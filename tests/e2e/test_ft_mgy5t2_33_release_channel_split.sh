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
import copy
import hashlib
import os
import shutil
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
    # Exercise the unchanged production verifier with an isolated trust root.
    # No signer secret or public-key override is added to the production CLI.
    fixture_root = td_path / "repo"
    (fixture_root / "scripts/release").mkdir(parents=True)
    (fixture_root / "release").mkdir()
    fixture_verify = fixture_root / "scripts/release/verify-release.sh"
    shutil.copyfile(verify_sh, fixture_verify)
    secret_key = td_path / "test.key"
    subprocess.run(
        ["minisign", "-G", "-W", "-s", str(secret_key),
         "-p", str(fixture_root / "release/minisign.pub")],
        check=True, capture_output=True, text=True,
    )
    assets = td_path / "assets"
    assets.mkdir()
    dmg = assets / "FrankenTerm-Exporter.dmg"
    app_zip = assets / "FrankenTerm-Exporter.app.zip"
    dmg.write_bytes(b"synthetic companion DMG for artifact-integrity verification\n")
    app_zip.write_bytes(b"synthetic companion app ZIP for artifact-integrity verification\n")
    sums = assets / "SHA256SUMS"

    def sign(path):
        subprocess.run(
            ["minisign", "-S", "-s", str(secret_key), "-m", str(path),
             "-x", str(path) + ".minisig", "-t", "frankenterm test fixture"],
            check=True, capture_output=True, text=True,
        )

    checksum_lines = []
    for artifact in (dmg, app_zip):
        digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
        line = f"{digest}  {artifact.name}\n"
        (assets / (artifact.name + ".sha256")).write_text(line)
        checksum_lines.append(line)
        sign(artifact)
    sums.write_text("".join(sorted(checksum_lines)))
    sign(sums)
    originals = {p.name: p.read_bytes() for p in assets.iterdir()}

    tag = "v0.15.22-appstore.16"
    good_json = td_path / "companion_good.json"
    good_companion = {
        "tag_name": tag,
        "draft": False,
        "prerelease": True,
        "target_commitish": "93d33742e75b063115a7460da9904e5f0d360303",
        "assets": [],
    }

    def refresh_metadata():
        good_companion["assets"] = [
            {"name": p.name, "size": p.stat().st_size,
             "digest": "sha256:" + hashlib.sha256(p.read_bytes()).hexdigest(),
             "browser_download_url": f"https://github.com/Dicklesworthstone/frankenterm/releases/download/{tag}/{p.name}"}
            for p in sorted(assets.iterdir())
        ]
        good_json.write_text(json.dumps(good_companion))

    def restore_assets():
        for name, data in originals.items():
            (assets / name).write_bytes(data)
        refresh_metadata()

    def companion_case(case_id, expected_code=None, *, metadata=good_json,
                       verifier=fixture_verify, extra_args=(), with_assets=True):
        before = {p.name: p.read_bytes() for p in assets.iterdir()}
        args = ["bash", str(verifier), "--channel", "companion-appstore", "--json",
                tag, "--release-json", str(metadata)]
        if with_assets:
            args += ["--assets-dir", str(assets)]
        p = subprocess.run(args + list(extra_args), capture_output=True, text=True)
        parsed = json.loads(p.stdout)
        expected_rc = 0 if expected_code is None else 1
        assert parsed.get("fail_code") == expected_code, (case_id, parsed, p.stderr)
        assert parsed.get("ok") == (expected_rc == 0), (case_id, parsed)
        after = {p.name: p.read_bytes() for p in assets.iterdir()}
        mutated_paths = sorted(name for name in before.keys() | after.keys()
                               if before.get(name) != after.get(name))
        assert not mutated_paths, (case_id, mutated_paths)
        record(case_id, "companion-appstore", "positive" if expected_rc == 0 else "negative",
               expected_rc, p, {"fail_code": parsed.get("fail_code"), "mutated_paths": mutated_paths})
        return parsed

    refresh_metadata()

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

    # 3. Real signatures and canonical newline-terminated checksum sidecars.
    parsed = companion_case("pos_companion_signed_artifacts")
    assert parsed["verification_scope"] == "companion_artifact_integrity", parsed
    companion_case("neg_companion_metadata_only", "missing_companion_assets", with_assets=False)
    companion_case("neg_companion_untrusted_signer", "minisig_invalid", verifier=verify_sh)

    # A one-byte mutation with refreshed public metadata must still fail the
    # trusted signature, even if the attacker supplies matching checksums.
    for artifact in (dmg, app_zip):
        original = artifact.read_bytes()
        artifact.write_bytes(bytes([original[0] ^ 1]) + original[1:])
        digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
        (assets / (artifact.name + ".sha256")).write_text(f"{digest}  {artifact.name}\n")
        sums.write_text("".join(sorted(
            f"{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.name}\n" for p in (dmg, app_zip)
        )))
        sign(sums)
        refresh_metadata()
        companion_case("neg_tampered_" + artifact.suffix[1:], "minisig_invalid")
        restore_assets()

    signature = assets / "FrankenTerm-Exporter.dmg.minisig"
    signature.write_bytes(b"invalid signature\n")
    refresh_metadata()
    companion_case("neg_invalid_companion_signature", "minisig_invalid")
    restore_assets()

    # Even a correctly signed aggregate must actually bind both artifacts.
    for case_id, contents in [
        ("neg_signed_aggregate_wrong_hash", "0" * 64 + f"  {dmg.name}\n"),
        ("neg_signed_aggregate_missing_dmg", checksum_lines[1]),
        ("neg_signed_aggregate_duplicate", "".join(checksum_lines) + checksum_lines[0]),
        ("neg_signed_aggregate_path_escape", "".join(checksum_lines) + "0" * 64 + "  ../escape\n"),
    ]:
        sums.write_text(contents)
        sign(sums)
        refresh_metadata()
        companion_case(case_id, "aggregate_checksum_mismatch")
        restore_assets()

    malformed_json = td_path / "companion_invalid.json"
    malformed = copy.deepcopy(good_companion)
    malformed["assets"].append(copy.deepcopy(malformed["assets"][0]))
    malformed_json.write_text(json.dumps(malformed))
    companion_case("neg_duplicate_companion_asset", "invalid_companion_metadata", metadata=malformed_json)
    malformed_json.write_text(good_json.read_text().replace('"draft": false', '"draft": true, "draft": false'))
    companion_case("neg_duplicate_companion_json_key", "invalid_companion_metadata", metadata=malformed_json)
    malformed = copy.deepcopy(good_companion)
    malformed["assets"] = [a for a in malformed["assets"] if a["name"] != app_zip.name + ".minisig"]
    malformed_json.write_text(json.dumps(malformed))
    companion_case("neg_app_zip_missing_signature", "missing_companion_asset", metadata=malformed_json)
    malformed = copy.deepcopy(good_companion)
    malformed["assets"][0]["size"] += 1
    malformed_json.write_text(json.dumps(malformed))
    companion_case("neg_companion_size_mismatch", "companion_size_mismatch", metadata=malformed_json)
    malformed["assets"][0]["size"] = 10 ** 100
    malformed_json.write_text(json.dumps(malformed))
    companion_case("neg_companion_unbounded_size", "invalid_companion_metadata", metadata=malformed_json)
    malformed = copy.deepcopy(good_companion)
    malformed["assets"][0]["digest"] = "sha256:" + "0" * 64
    malformed_json.write_text(json.dumps(malformed))
    companion_case("neg_companion_digest_mismatch", "companion_digest_mismatch", metadata=malformed_json)
    companion_case("neg_companion_ignored_manifest", "unsupported_companion_option",
                   extra_args=("--manifest", "requested-manifest.json"))
    companion_case("neg_companion_ignored_attestation", "unsupported_companion_option",
                   extra_args=("--attestation-bundle", "requested-bundle.json", "--release-policy", "requested-policy.json"))

    # Test the real online download branch with isolated HTTP transports, while
    # keeping the production digest and Minisign verification fully active.
    bin_dir = td_path / "bin"
    bin_dir.mkdir()
    (bin_dir / "gh").write_text("#!/bin/sh\ncat \"$FT_TEST_RELEASE_JSON\"\n")
    (bin_dir / "curl").write_text('''#!/usr/bin/env python3
import os, pathlib, shutil, sys
args = sys.argv[1:]
assert "--max-filesize" in args and "--max-time" in args
assert args[args.index("--proto") + 1] == "=https"
target = pathlib.Path(args[args.index("--output") + 1])
source = pathlib.Path(os.environ["FT_TEST_ASSETS"]) / args[-1].rsplit("/", 1)[1]
with open(os.environ["FT_TEST_DOWNLOAD_LOG"], "a") as log:
    log.write(source.name + "\\n")
shutil.copyfile(source, target)
''')
    for tool in bin_dir.iterdir():
        tool.chmod(0o755)
    download_log = td_path / "downloads.log"
    env = dict(os.environ, PATH=str(bin_dir) + os.pathsep + os.environ["PATH"],
               FT_TEST_RELEASE_JSON=str(good_json), FT_TEST_ASSETS=str(assets),
               FT_TEST_DOWNLOAD_LOG=str(download_log), TMPDIR=str(td_path))
    p = subprocess.run(["bash", str(fixture_verify), "--channel", "companion-appstore", "--json", tag],
                       env=env, capture_output=True, text=True)
    parsed = json.loads(p.stdout)
    assert parsed.get("ok") is True, (parsed, p.stderr)
    assert sorted(download_log.read_text().splitlines()) == sorted(originals)
    record("pos_online_companion_downloads_and_authenticates", "companion-appstore", "positive", 0, p)

    malformed = copy.deepcopy(good_companion)
    malformed["assets"][0]["browser_download_url"] = "https://untrusted.invalid/artifact"
    malformed_json.write_text(json.dumps(malformed))
    env["FT_TEST_RELEASE_JSON"] = str(malformed_json)
    p = subprocess.run(["bash", str(fixture_verify), "--channel", "companion-appstore", "--json", tag],
                       env=env, capture_output=True, text=True)
    parsed = json.loads(p.stdout)
    assert parsed.get("fail_code") == "invalid_companion_asset", (parsed, p.stderr)
    record("neg_online_companion_foreign_download_url", "companion-appstore", "negative", 1, p)

    # 4. Negative: companion-appstore fixture with prerelease=false (hijacking GitHub Latest!)
    bad_companion = dict(good_companion, prerelease=False)
    bad_json = td_path / "companion_not_prerelease.json"
    bad_json.write_text(json.dumps(bad_companion))
    companion_case("neg_companion_hijacks_latest", "companion_release_not_prerelease", metadata=bad_json)

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

    # Exercise the complete resolver, including HTTP failure, pagination and
    # redirect fallback. Only curl is stubbed; production selection runs with
    # errexit/nounset/pipefail, and every requested URL is checked below.
    install_source = install_sh.read_text()

    def install_function(name):
        start = install_source.index(name + "() {")
        end = install_source.index("\n}\n", start) + 3
        return install_source[start:end]

    resolver_script = "\n".join(install_function(name) for name in
                                ("release_tag_with_assets", "resolve_version")) + r'''
set -euo pipefail
PROXY_ARGS=()
info() { printf '%s\n' "$*" >&2; }
err() { printf '%s\n' "$*" >&2; }
if resolve_version; then
  printf '%s\n' "$VERSION"
else
  exit 1
fi
'''
    resolver_bin = td_path / "resolver_bin"
    resolver_bin.mkdir()
    (resolver_bin / "curl").write_text('''#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
with open(os.environ["FT_TEST_RESOLVER_LOG"], "a") as log:
    log.write(json.dumps(args) + "\\n")
with open(os.environ["FT_TEST_RESOLVER_RESPONSES"]) as source:
    responses = json.load(source)
response = responses.get(args[-1], {"status": 99})
body = response.get("body", "")
sys.stdout.write(json.dumps(body) if isinstance(body, (dict, list)) else body)
sys.exit(response.get("status", 0))
''')
    (resolver_bin / "curl").chmod(0o755)
    resolver_responses = td_path / "resolver_responses.json"
    resolver_log = td_path / "resolver_requests.jsonl"
    api = "https://api.github.com/repos/fixture/frankenterm/releases"
    latest_url = api + "/latest"
    redirect_url = "https://github.com/fixture/frankenterm/releases/latest"
    stable_tag = releases_fixture[1]["tag_name"]
    asset_name = "ft-linux-amd64.tar.xz"
    asset_base = f"https://github.com/fixture/frankenterm/releases/download/{stable_tag}/"

    def page_url(number):
        return api + f"?per_page=100&page={number}"

    def resolver_case(case_id, responses, requested_urls, *, version=None,
                      error=None, overrides=None):
        resolver_responses.write_text(json.dumps(responses))
        resolver_log.write_text("")
        env = dict(os.environ, PATH=str(resolver_bin) + os.pathsep + os.environ["PATH"],
                   FT_TEST_RESOLVER_RESPONSES=str(resolver_responses),
                   FT_TEST_RESOLVER_LOG=str(resolver_log), VERSION="", FROM_SOURCE="0",
                   ARTIFACT_URL="", ASSET=asset_name, CHECKSUM="", CHECKSUM_URL="",
                   MINISIGN_SIGNATURE_URL="", NO_MINISIGN="0", OWNER="fixture",
                   REPO="frankenterm", OS="linux", ARCH="x86_64")
        env.update(overrides or {})
        p = subprocess.run(["bash", "-c", resolver_script], env=env,
                           capture_output=True, text=True, timeout=15)
        requests = [json.loads(line) for line in resolver_log.read_text().splitlines()]
        assert [args[-1] for args in requests] == requested_urls, (case_id, requests, p.stderr)
        for args in requests:
            assert "--max-time" in args, (case_id, args)
            assert 0 < int(args[args.index("--max-time") + 1]) <= 15, (case_id, args)
        assert p.stdout.strip() == (version or ""), (case_id, p.stdout, p.stderr)
        if error:
            assert error in p.stderr, (case_id, p.stderr)
        if error and "No published stable" not in error:
            assert "No published stable" not in p.stderr, (case_id, p.stderr)
        record(case_id, "stable", "positive" if version else "negative",
               0 if version else 1, p,
               {"selected_tag": p.stdout.strip(), "request_count": len(requests)})

    resolver_case("pos_resolver_latest_signed", {latest_url: {"body": releases_fixture[1]}},
                  [latest_url], version=stable_tag)
    for case_id, latest_response in [
        ("pos_resolver_missing_latest_uses_list", {"status": 22}),
        ("pos_resolver_malformed_latest_uses_list", {"body": "{broken"}),
    ]:
        resolver_case(case_id, {latest_url: latest_response,
                               page_url(1): {"body": [releases_fixture[1]]}},
                      [latest_url, page_url(1)], version=stable_tag)

    companion_page = [dict(releases_fixture[0], tag_name=f"v0.15.22-appstore.{i}")
                      for i in range(100)]
    resolver_case("pos_resolver_finds_stable_on_page_two", {
        latest_url: {"body": releases_fixture[0]},
        page_url(1): {"body": companion_page},
        page_url(2): {"body": [releases_fixture[1]]},
    }, [latest_url, page_url(1), page_url(2)], version=stable_tag)
    resolver_case("neg_resolver_page_limit_is_not_exhaustion", {
        latest_url: {"status": 22},
        **{page_url(i): {"body": companion_page} for i in range(1, 11)},
    }, [latest_url] + [page_url(i) for i in range(1, 11)], error="limit of 10 pages")

    for suffix, releases in [("empty", []), ("companion_only", [releases_fixture[0]])]:
        resolver_case("neg_resolver_exhausted_" + suffix, {
            latest_url: {"status": 22}, page_url(1): {"body": releases},
        }, [latest_url, page_url(1)], error="No published stable release ships a verifiable")
    for suffix, response, error in [
        ("transport", {"status": 22}, "could not fetch release-list page 1"),
        ("api_error_object", {"body": {"message": "rate limited"}}, "invalid release list on page 1"),
        ("malformed", {"body": "{broken"}, "invalid release list on page 1"),
    ]:
        resolver_case("neg_resolver_list_" + suffix, {
            latest_url: {"status": 22}, page_url(1): response, redirect_url: {"status": 22},
        }, [latest_url, page_url(1), redirect_url], error=error)
    resolver_case("neg_resolver_partial_list_failure", {
        latest_url: {"body": releases_fixture[0]}, page_url(1): {"body": companion_page},
        page_url(2): {"status": 28}, redirect_url: {"status": 22},
    }, [latest_url, page_url(1), page_url(2), redirect_url], error="could not fetch release-list page 2")

    fallback = {latest_url: {"status": 22}, page_url(1): {"status": 22},
                redirect_url: {"body": f"https://github.com/fixture/frankenterm/releases/tag/{stable_tag}"}}
    fallback_requests = [latest_url, page_url(1), redirect_url]
    required_files = [asset_name, asset_name + ".sha256", asset_name + ".minisig"]
    resolver_case("pos_resolver_redirect_requires_all_files", {
        **fallback, **{asset_base + name: {} for name in required_files},
    }, fallback_requests + [asset_base + name for name in required_files], version=stable_tag)
    for missing_index in (1, 2):
        requested = required_files[:missing_index + 1]
        resolver_case("neg_resolver_redirect_missing_" + ("checksum" if missing_index == 1 else "signature"), {
            **fallback, **{asset_base + name: {} for name in requested},
            asset_base + requested[-1]: {"status": 22},
        }, fallback_requests + [asset_base + name for name in requested],
            error="cannot provide required file " + requested[-1])
    for suffix, overrides, needed_files in [
        ("checksum", {"CHECKSUM": "a" * 64}, [asset_name, asset_name + ".minisig"]),
        ("checksum_url_no_verify", {"CHECKSUM_URL": "https://fixture.invalid/checksum", "NO_MINISIGN": "1"}, [asset_name]),
        ("no_verify", {"NO_MINISIGN": "1"}, [asset_name, asset_name + ".sha256"]),
        ("signature_url", {"MINISIGN_SIGNATURE_URL": "https://fixture.invalid/signature"}, [asset_name, asset_name + ".sha256"]),
    ]:
        resolver_case("pos_resolver_redirect_override_" + suffix, {
            **fallback, **{asset_base + name: {} for name in needed_files},
        }, fallback_requests + [asset_base + name for name in needed_files],
            version=stable_tag, overrides=overrides)
    for suffix, overrides in [("source", {"FROM_SOURCE": "1"}),
                              ("custom_artifact", {"ARTIFACT_URL": "https://fixture.invalid/archive.tar.xz"})]:
        resolver_case("pos_resolver_" + suffix + "_needs_no_release_assets", {
            latest_url: {"status": 22},
            page_url(1): {"body": [dict(releases_fixture[1], assets=[])]},
        }, [latest_url, page_url(1)], version=stable_tag, overrides=overrides)
    resolver_case("pos_resolver_explicit_version_skips_network", {}, [],
                  version=releases_fixture[0]["tag_name"],
                  overrides={"VERSION": releases_fixture[0]["tag_name"]})
    resolver_case("neg_resolver_redirect_refuses_companion", {
        **fallback, redirect_url: {"body": "https://github.com/fixture/frankenterm/releases/tag/v0.15.22-appstore.15"},
    }, fallback_requests, error="non-stable companion/prerelease tag")

    # 9. Negative: 1-byte tamper in offline archive fails closed with zero mutation to --dest
    import hashlib, tarfile
    dest_dir = td_path / "dest_bin"
    dest_dir.mkdir()
    sentinel = dest_dir / "existing_marker"
    sentinel.write_text("unchanged-before-install\n")
    fake_bin = td_path / "ft"
    fake_bin.write_text("#!/bin/sh\necho fake\n")
    good_tar = td_path / "ft-test.tar.xz"
    with tarfile.open(good_tar, "w:xz") as tf:
        tf.add(fake_bin, arcname="ft")
    orig_sha = hashlib.sha256(good_tar.read_bytes()).hexdigest()
    tampered_bytes = bytearray(good_tar.read_bytes())
    tampered_bytes[-1] ^= 0x01
    bad_tar = td_path / "ft-test-tampered.tar.xz"
    bad_tar.write_bytes(bytes(tampered_bytes))
    env = dict(os.environ, HOME=str(td_path / "fake_home"))
    Path(env["HOME"]).mkdir()
    p = subprocess.run(
        ["bash", str(install_sh), "--offline", str(bad_tar), "--checksum", orig_sha,
         "--no-verify", "--no-app", "--dest", str(dest_dir), "--quiet"],
        capture_output=True, text=True, env=env,
    )
    after_files = sorted(f.name for f in dest_dir.iterdir())
    mutated = (after_files != ["existing_marker"]) or (sentinel.read_text() != "unchanged-before-install\n")
    ok_tamper = (p.returncode != 0) and (not mutated)
    results.append({
        "case_id": "neg_install_1byte_tamper_zero_mutation",
        "channel": "stable",
        "case_kind": "negative",
        "expected_exit": "non-zero",
        "actual_exit": p.returncode,
        "mutated_paths": [] if not mutated else after_files,
        "verdict": "pass" if ok_tamper else "fail",
    })

    # 10. Syntax check on install.sh & verify-release.sh (bash -n)
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
