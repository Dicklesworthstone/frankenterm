#!/usr/bin/env python3
"""Unit tests for scripts/ghostty_h2h.py (ft-yccm0.1.3).

Run: python3 -I scripts/test_ghostty_h2h.py   (also run by
scripts/test_ghostty_headless_h2h.sh). Stdlib only, no Ghostty or zig.
"""

from __future__ import annotations

import importlib.util
import json
import math
import statistics
import sys
import tempfile
import types
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent
REPO = SCRIPTS.parent
CONTRACT = REPO / "docs" / "perf" / "incumbents" / "ghostty.md"


def _load_helper() -> types.ModuleType:
    spec = importlib.util.spec_from_file_location("ghostty_h2h", SCRIPTS / "ghostty_h2h.py")
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["ghostty_h2h"] = module
    spec.loader.exec_module(module)
    return module


h2h = _load_helper()


def pin_block(**overrides: str) -> str:
    """A complete pin block built from the real contract, with overrides."""
    pins = {}
    for line in h2h.extract_pin_block(CONTRACT.read_text()):
        text = line.strip()
        if text and not text.startswith("#"):
            key, _, value = text.partition("=")
            pins[key.strip()] = value.strip()
    pins.update(overrides)
    body = "\n".join(f"{key} = {value}" for key, value in pins.items() if value is not None)
    return f"# contract\n\n```{h2h.PIN_FENCE}\n{body}\n```\n"


def gates(**overrides):
    base = {"max_cv_pct": 5.0, "max_load_1m": 4.0, "min_runs": 10, "rounds": 10, "warmup": 3, "order": "ABBA"}
    base.update(overrides)
    return base


def stats_for(ghostty: list[float], ft: list[float]):
    return {"ghostty": h2h.arm_stats(ghostty), "frankenterm": h2h.arm_stats(ft)}


def hyperfine_round(path: Path, order: list[str], times: dict[str, list[float]], codes=None):
    results = []
    for arm in order:
        results.append(
            {
                "command": arm,
                "mean": times[arm][0],
                "stddev": None,
                "median": times[arm][0],
                "user": 0.0,
                "system": 0.0,
                "min": min(times[arm]),
                "max": max(times[arm]),
                "times": times[arm],
                "memory_usage_byte": [1 << 20],
                "exit_codes": (codes or {}).get(arm, [0] * len(times[arm])),
            }
        )
    path.write_text(json.dumps({"results": results}))


def ft_record(**overrides):
    record = {
        "schema": h2h.FT_LANE_SCHEMA,
        "lane": "term",
        "rows": 80,
        "cols": 120,
        "scrollback": 3500,
        "chunk": 65536,
        "cursor_x": 8,
        "cursor_y": 79,
        "retained_rows": 617,
        "sanity": "ok",
        "debug_assertions": False,
        "profile": "release-perf",
        "state_fingerprint": "1a4dca3c3d283e93",
        "secs": 0.0149,
    }
    record.update(overrides)
    return record


def probe_record(**overrides):
    record = {
        "schema": h2h.PROBE_SCHEMA,
        "bytes": 1048576,
        "rows": 80,
        "cols": 120,
        "read_chunk": 65536,
        "cursor_x": 8,
        "cursor_y": 79,
        "pending_wrap": False,
        "total_rows": 617,
        "wrapped_rows": 616,
        "bench_equivalent_total_rows": 617,
    }
    record.update(overrides)
    return record


class ContractPins(unittest.TestCase):
    def test_the_committed_contract_pins_the_planning_session_facts(self):
        pins = h2h.parse_pins(CONTRACT.read_text())
        self.assertEqual(pins["ghostty_commit"], "e500d414f2ef688d86d0228f39e8ca1a1285f72a")
        self.assertEqual(pins["zig_version"], "0.16.0")
        self.assertEqual(
            pins["zig_tarball_sha256"],
            "b23d70deaa879b5c2d486ed3316f7eaa53e84acf6fc9cc747de152450d401489",
        )
        self.assertEqual(pins["zig_tarball"], "zig-aarch64-macos-0.16.0.tar.xz")
        flags = pins["build_flags"].split()
        for flag in ("-Demit-bench", "-Doptimize=ReleaseFast", "-Demit-macos-app=false"):
            self.assertIn(flag, flags)
        self.assertEqual(pins["bench_action"], "+terminal-stream")
        self.assertEqual(pins["geometries"], [{"rows": 80, "cols": 120}, {"rows": 117, "cols": 512}])
        self.assertEqual(pins["primary_corpus"], "color_emoji_random")
        self.assertEqual((pins["ft_profile"], pins["ft_scrollback"]), ("release-perf", 3500))
        # The FT arm feeds what ghostty-bench reads per call.
        self.assertEqual(pins["ft_chunk_bytes"], pins["bench_read_chunk_bytes"])
        self.assertEqual((pins["min_runs"], pins["max_cv_pct"], pins["max_load_1m"]), (10, 5.0, 4.0))

    def test_a_complete_block_round_trips(self):
        self.assertEqual(h2h.parse_pins(pin_block()), h2h.parse_pins(CONTRACT.read_text()))

    def test_missing_unknown_duplicate_and_malformed_pins_fail_closed(self):
        with self.assertRaisesRegex(h2h.PinError, "missing pins: ghostty_commit"):
            h2h.parse_pins(pin_block(ghostty_commit=None))
        with self.assertRaisesRegex(h2h.PinError, "unknown pin 'surprise'"):
            h2h.parse_pins(pin_block().replace("\n```\n", "\nsurprise = 1\n```\n"))
        block = pin_block()
        duplicated = block.replace("zig_version = 0.16.0", "zig_version = 0.16.0\nzig_version = 0.16.0")
        with self.assertRaisesRegex(h2h.PinError, "set twice"):
            h2h.parse_pins(duplicated)
        with self.assertRaisesRegex(h2h.PinError, "ghostty_commit"):
            h2h.parse_pins(pin_block(ghostty_commit="e500d414f"))
        with self.assertRaisesRegex(h2h.PinError, "zig_tarball_sha256"):
            h2h.parse_pins(pin_block(zig_tarball_sha256="B23D" + "0" * 60))
        with self.assertRaisesRegex(h2h.PinError, "geometries"):
            h2h.parse_pins(pin_block(geometries="80x120 117by512"))
        with self.assertRaisesRegex(h2h.PinError, "ft_profile"):
            h2h.parse_pins(pin_block(ft_profile="release"))
        with self.assertRaisesRegex(h2h.PinError, "build_flags"):
            h2h.parse_pins(pin_block(build_flags="-Demit-bench --verbose"))
        with self.assertRaisesRegex(h2h.PinError, "must end with the zig_tarball"):
            h2h.parse_pins(pin_block(zig_tarball_url="https://ziglang.org/download/0.16.0/other.tar.xz"))

    def test_the_block_must_exist_once_and_be_closed(self):
        with self.assertRaisesRegex(h2h.PinError, "no ```"):
            h2h.parse_pins("# nothing here\n")
        with self.assertRaisesRegex(h2h.PinError, "not closed"):
            h2h.parse_pins(f"```{h2h.PIN_FENCE}\nzig_version = 0.16.0\n")
        with self.assertRaisesRegex(h2h.PinError, "exactly one"):
            h2h.parse_pins(pin_block() + pin_block())

    def test_pin_as_text_formats_geometries_for_the_shell(self):
        pins = h2h.parse_pins(CONTRACT.read_text())
        self.assertEqual(h2h.pin_as_text(pins, "geometries"), "80x120 117x512")
        self.assertEqual(h2h.pin_as_text(pins, "min_runs"), "10")
        with self.assertRaises(h2h.UsageError):
            h2h.pin_as_text(pins, "nope")


class Gates(unittest.TestCase):
    def setUp(self):
        self.pins = h2h.parse_pins(CONTRACT.read_text())

    def test_defaults_come_from_the_contract(self):
        resolved = h2h.resolve_gates(self.pins, None, None, None, None, False)
        self.assertEqual((resolved["rounds"], resolved["warmup"], resolved["order"]), (10, 3, "ABBA"))

    def test_overrides_may_tighten(self):
        resolved = h2h.resolve_gates(self.pins, 2.5, 1.0, 20, 5, False)
        self.assertEqual((resolved["max_cv_pct"], resolved["max_load_1m"], resolved["rounds"]), (2.5, 1.0, 20))
        self.assertEqual(resolved["contract_max_load_1m"], 4.0)

    def test_overrides_never_loosen(self):
        for kwargs in ({"max_cv": 6.0}, {"max_load": 4.5}, {"max_cv": 0.0}, {"max_load": float("nan")}):
            with self.subTest(kwargs=kwargs), self.assertRaises(h2h.UsageError):
                h2h.resolve_gates(self.pins, kwargs.get("max_cv"), kwargs.get("max_load"), None, None, False)
        with self.assertRaisesRegex(h2h.UsageError, "min_runs"):
            h2h.resolve_gates(self.pins, None, None, 9, None, False)
        with self.assertRaises(h2h.UsageError):
            h2h.resolve_gates(self.pins, None, None, 1, None, True)

    def test_the_self_test_may_run_fewer_rounds_than_a_verdict_needs(self):
        self.assertEqual(h2h.resolve_gates(self.pins, None, None, 2, 1, True)["rounds"], 2)


class Statistics(unittest.TestCase):
    def test_nearest_rank_p95(self):
        values = [float(x) for x in range(1, 11)]
        self.assertEqual(h2h.percentile_nearest_rank(values, 95), 10.0)
        self.assertEqual(h2h.percentile_nearest_rank(values, 50), 5.0)
        self.assertEqual(h2h.percentile_nearest_rank([3.0, 1.0, 2.0], 95), 3.0)
        twenty = [float(x) for x in range(1, 21)]
        self.assertEqual(h2h.percentile_nearest_rank(twenty, 95), 19.0)

    def test_arm_stats(self):
        times = [1.0, 1.1, 0.9, 1.0]
        stats = h2h.arm_stats(times)
        self.assertEqual(stats["n"], 4)
        self.assertAlmostEqual(stats["median_s"], 1.0)
        self.assertAlmostEqual(stats["cv_pct"], 100 * statistics.stdev(times) / statistics.fmean(times))
        self.assertEqual((stats["min_s"], stats["max_s"], stats["p95_s"]), (0.9, 1.1, 1.1))


class Decide(unittest.TestCase):
    QUIET = 1.5

    def test_admits_a_quiet_low_variance_row(self):
        verdict, speedup, reasons = h2h.decide(
            stats_for([0.50] * 9 + [0.51], [1.00] * 9 + [1.01]), gates(), self.QUIET, []
        )
        self.assertEqual((verdict, reasons), ("ghostty_faster", []))
        self.assertAlmostEqual(speedup, 0.5, places=3)
        verdict, speedup, _ = h2h.decide(
            stats_for([1.00] * 10, [0.40] * 9 + [0.41]), gates(), self.QUIET, []
        )
        self.assertEqual(verdict, "ft_faster")
        self.assertGreater(speedup, 2.0)

    def test_refuses_noise(self):
        verdict, speedup, _ = h2h.decide(
            stats_for([1.0, 1.2] * 5, [0.4] * 10), gates(), self.QUIET, []
        )
        self.assertIsNone(speedup)
        self.assertRegex(verdict, r"^NO_ADMISSIBLE_RATIO \(cv: ghostty \d+\.\d\d% > 5%\)$")

    def test_refuses_a_loaded_host_or_an_unknown_load(self):
        verdict, _, _ = h2h.decide(stats_for([1.0] * 10, [0.5] * 10), gates(), 9.12, [])
        self.assertEqual(verdict, "NO_ADMISSIBLE_RATIO (load: 1m load average peaked at 9.12 > 4)")
        verdict, _, _ = h2h.decide(stats_for([1.0] * 10, [0.5] * 10), gates(), None, [])
        self.assertIn("load average could not be read", verdict)

    def test_refuses_too_few_runs_and_reports_every_reason(self):
        verdict, speedup, reasons = h2h.decide(
            stats_for([1.0, 1.5], [0.5, 0.5]), gates(), 7.0, ["ft sanity: no visible text"]
        )
        self.assertIsNone(speedup)
        self.assertTrue(verdict.startswith("NO_ADMISSIBLE_RATIO (runs: ghostty n=2, frankenterm n=2 < min_runs 10;"))
        self.assertEqual(len(reasons), 4)
        self.assertIn("ft sanity: no visible text", verdict)

    def test_refuses_a_tie(self):
        verdict, _, _ = h2h.decide(stats_for([1.0] * 10, [1.0] * 10), gates(), self.QUIET, [])
        self.assertEqual(verdict, "NO_ADMISSIBLE_RATIO (tie: equal medians)")

    def test_every_verdict_matches_the_published_forms(self):
        for verdict in ("ft_faster", "ghostty_faster", "NO_ADMISSIBLE_RATIO (cv: x)"):
            self.assertRegex(verdict, h2h.VERDICT_RE)
        for verdict in ("mixed", "NO_ADMISSIBLE_RATIO", "NO_ADMISSIBLE_RATIO ()", "ft_faster!"):
            self.assertNotRegex(verdict, h2h.VERDICT_RE)


class Rows(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def write_rounds(self, rounds, ghostty=0.45, ft=0.90):
        for index in range(1, rounds + 1):
            order = list(h2h.ARMS) if index % 2 else list(reversed(h2h.ARMS))
            hyperfine_round(
                self.dir / f"round-{index:02d}.json",
                order,
                {"ghostty": [ghostty + index * 1e-4], "frankenterm": [ft + index * 1e-4]},
            )

    def write_admission(self, pre=None, post=None):
        (self.dir / "ft-admission-pre.jsonl").write_text(json.dumps(pre or ft_record()) + "\n")
        (self.dir / "ft-admission-post.jsonl").write_text(json.dumps(post or ft_record()) + "\n")

    def row_args(self, rounds=10, **overrides):
        gates_path = self.dir / "gates.json"
        gates_path.write_text(json.dumps(gates(rounds=rounds)))
        fields = {
            "dir": str(self.dir), "gates": str(gates_path), "corpus": "color_emoji_random",
            "input_sha256": "a" * 64, "rows": 80, "cols": 120, "ft_scrollback": 3500,
            "ft_chunk": 65536, "ft_profile": "release-perf", "ghostty_cmd": "g", "ft_cmd": "f",
        }
        fields.update(overrides)
        return types.SimpleNamespace(**fields)

    def test_abba_order_one_run_per_arm_and_clean_exits_are_enforced(self):
        self.write_rounds(4)
        times, per_round = h2h.read_rounds(self.dir, 4)
        self.assertEqual([entry["order"][0] for entry in per_round], ["ghostty", "frankenterm"] * 2)
        self.assertEqual(len(times["ghostty"]), 4)
        hyperfine_round(self.dir / "round-02.json", list(h2h.ARMS), {"ghostty": [1.0], "frankenterm": [1.0]})
        with self.assertRaisesRegex(h2h.UsageError, "ABBA expects"):
            h2h.read_rounds(self.dir, 4)
        hyperfine_round(self.dir / "round-02.json", ["frankenterm", "ghostty"], {"ghostty": [1.0, 1.1], "frankenterm": [1.0]})
        with self.assertRaisesRegex(h2h.UsageError, "exactly one timed run"):
            h2h.read_rounds(self.dir, 4)
        hyperfine_round(self.dir / "round-02.json", ["frankenterm", "ghostty"], {"ghostty": [1.0], "frankenterm": [1.0]}, codes={"ghostty": [1]})
        with self.assertRaisesRegex(h2h.UsageError, "exited"):
            h2h.read_rounds(self.dir, 4)
        with self.assertRaisesRegex(h2h.UsageError, "missing hyperfine exports"):
            h2h.read_rounds(self.dir, 5)

    def test_admission_problems(self):
        self.write_admission()
        records, problems = h2h.read_ft_admission(self.dir, "release-perf", 80, 120)
        self.assertEqual((len(records), problems), (2, []))
        self.write_admission(post=ft_record(state_fingerprint="ffff"))
        _, problems = h2h.read_ft_admission(self.dir, "release-perf", 80, 120)
        self.assertEqual(problems, ["ft state: the runs before and after the rounds left different final states"])
        self.write_admission(pre=ft_record(profile="release"), post=ft_record(profile="release"))
        _, problems = h2h.read_ft_admission(self.dir, "release-perf", 80, 120)
        self.assertEqual(problems, ["ft build: profile 'release', expected 'release-perf'"])
        self.write_admission(pre=ft_record(sanity="no visible text"), post=ft_record(debug_assertions=True))
        _, problems = h2h.read_ft_admission(self.dir, "release-perf", 80, 120)
        self.assertIn("ft sanity: no visible text", problems)
        self.assertIn("ft build: debug assertions are on", problems)
        _, problems = h2h.read_ft_admission(self.dir, "release-perf", 117, 512)
        self.assertTrue(any("expected 117x512" in p for p in problems))

    def test_build_row_folds_rounds_load_and_admission_into_one_verdict(self):
        self.write_rounds(10)
        self.write_admission()
        (self.dir / "load.txt").write_text("".join(f"2026-10-06T00:00:{i:02d}Z 1.{i}0 2.0 3.0\n" for i in range(11)))
        row = h2h.build_row(self.row_args())
        self.assertEqual(row["verdict"], "ghostty_faster")
        self.assertAlmostEqual(row["ft_speedup"], row["arms"]["ghostty"]["median_s"] / row["arms"]["frankenterm"]["median_s"])
        self.assertEqual(row["paired"], {"pairs": 10, "ft_wins": 0, "median_pair_speedup": row["paired"]["median_pair_speedup"]})
        self.assertAlmostEqual(row["load_1m"]["peak"], 1.9)
        self.assertEqual(row["arms"]["frankenterm"]["state_fingerprints"], ["1a4dca3c3d283e93"])
        self.assertEqual(row["arms"]["frankenterm"]["internal_lane_secs_untimed_runs"], [0.0149, 0.0149])
        self.assertEqual(len(row["hyperfine_json"]), 10)

    def test_a_loaded_or_unreadable_load_sample_refuses_the_row(self):
        self.write_rounds(10)
        self.write_admission()
        (self.dir / "load.txt").write_text("t0 1.0 1 1\nt1 unknown\n")
        row = h2h.build_row(self.row_args())
        self.assertIn("load average could not be read", row["verdict"])
        self.assertIsNone(row["ft_speedup"])
        self.assertIsNotNone(row["ft_speedup_unadmitted"])


class Parity(unittest.TestCase):
    def test_written_rows(self):
        self.assertEqual(h2h.written_rows(80, 80, 0), 1)
        self.assertEqual(h2h.written_rows(80, 80, 79), 80)
        self.assertEqual(h2h.written_rows(617, 80, 79), 617)

    def test_agreement(self):
        result = h2h.compare_parity(probe_record(), ft_record(), 80, 120, 20000, 0)
        self.assertTrue(result["agree"])
        self.assertEqual(result["ghostty"]["wraps"], 616)
        self.assertTrue(result["ghostty_wrap_flags_consistent"])

    def test_the_measured_one_mib_disagreement_is_recorded(self):
        # The numbers the probe and ingest_throughput produced on a 1 MiB slice
        # of color_emoji_random at 80x120 (ft-b35o7).
        result = h2h.compare_parity(
            probe_record(), ft_record(cursor_x=3, retained_rows=611), 80, 120, 20000, 0
        )
        self.assertFalse(result["agree"])
        self.assertEqual(
            result["disagreements"],
            [
                "rows written: ghostty 617, frankenterm 611 (wraps 616 vs 610)",
                "final cursor column: ghostty 8, frankenterm 3",
            ],
        )

    def test_a_saturated_scrollback_is_undecided(self):
        result = h2h.compare_parity(probe_record(), ft_record(retained_rows=180), 80, 120, 100, 0)
        self.assertIsNone(result["agree"])
        self.assertIn("undecided", result["disagreements"][0])

    def test_geometry_and_schema_mismatches_are_errors(self):
        with self.assertRaisesRegex(h2h.UsageError, "expected 117x512"):
            h2h.compare_parity(probe_record(), ft_record(), 117, 512, 20000, 0)
        with self.assertRaisesRegex(h2h.UsageError, "schema"):
            h2h.compare_parity(probe_record(schema="x"), ft_record(), 80, 120, 20000, 0)


class InputsAndTrees(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def generated(self, name, content, sha=None):
        path = self.dir / f"{name}.bin"
        path.write_bytes(content)
        return {
            "schema": "ft.bench.ingest-corpus.v1", "corpus": name, "corpus_path": str(path),
            "corpus_sha256": sha or h2h.sha256_file(path), "seed": 7, "bytes": len(content),
        }

    def test_primary_first_and_independent_hashes(self):
        records = [self.generated("seq_lines", b"1\n2\n"), self.generated("color_emoji_random", b"\x1b[38;5;1mA")]
        extra = self.dir / "operator.bin"
        extra.write_bytes(b"xyz")
        inputs = h2h.collect_inputs(records, [extra], "color_emoji_random")
        self.assertEqual([entry["corpus"] for entry in inputs], ["color_emoji_random", "seq_lines", "file:operator.bin"])
        self.assertEqual(inputs[2]["bytes"], 3)
        bad = [self.generated("seq_lines", b"1\n", sha="0" * 64)]
        with self.assertRaisesRegex(h2h.UsageError, "differs from the generator"):
            h2h.collect_inputs(bad, [], "color_emoji_random")
        with self.assertRaisesRegex(h2h.UsageError, "empty inputs"):
            h2h.collect_inputs([self.generated("seq_lines", b"")], [], "x")

    def test_tree_digest_notices_a_missing_or_changed_file(self):
        root = self.dir / "zig"
        (root / "lib" / "std" / "Target").mkdir(parents=True)
        (root / "zig").write_bytes(b"binary")
        target = root / "lib" / "std" / "Target" / "x86.zig"
        target.write_text("pub const cpu = 1;\n")
        count, digest = h2h.tree_digest(root)
        self.assertEqual(count, 2)
        self.assertEqual(h2h.tree_digest(root), (count, digest))
        target.write_text("pub const cpu = 12;\n")
        self.assertNotEqual(h2h.tree_digest(root)[1], digest)
        target.unlink()
        self.assertEqual(h2h.tree_digest(root)[0], 1)

    def test_app_drift_names_every_field(self):
        pins = h2h.parse_pins(CONTRACT.read_text())
        info = {"bundle_id": pins["app_bundle_id"], "short_version": "1.2.0", "build": pins["app_build"], "binary_sha256": "0" * 64}
        drift = h2h.app_drift(pins, info)
        self.assertEqual(len(drift), 2)
        self.assertTrue(drift[0].startswith("short_version is '1.2.0'"))


def valid_receipt(root: Path) -> dict:
    pins = h2h.parse_pins(CONTRACT.read_text())
    row_dir = root / "rows" / "color_emoji_random@80x120"
    row_dir.mkdir(parents=True)
    (row_dir / "round-01.json").write_text("{}")
    sha = "b" * 64
    row = {
        "schema": h2h.ROW_SCHEMA, "corpus": "color_emoji_random", "input_sha256": sha,
        "geometry": {"rows": 80, "cols": 120}, "dir": "rows/color_emoji_random@80x120",
        "arms": {arm: h2h.arm_stats([0.5, 0.51]) for arm in h2h.ARMS},
        "hyperfine_json": ["round-01.json"], "verdict": "ft_faster", "ft_speedup": 1.5,
    }
    parity = h2h.compare_parity(probe_record(), ft_record(), 80, 120, 20000, 0)
    binary = {"path": "/x", "sha256": "c" * 64}
    return {
        "schema": h2h.RECEIPT_SCHEMA, "mode": "measure", "started_utc": "t0", "finished_utc": "t1",
        "contract": {"path": str(CONTRACT), "sha256": "d" * 64}, "pins": pins,
        "gates": gates(), "fingerprint": {},
        "binaries": {"ghostty_bench": binary, "frankenterm": binary, "ghostty_probe": binary},
        "ghostty": {"commit": pins["ghostty_commit"], "checkout_clean": True},
        "inputs": [{"corpus": "color_emoji_random", "path": "/c", "bytes": 10, "sha256": sha}],
        "rows": [row], "emoji_width_parity": [parity],
        "primary": {"corpus": "color_emoji_random"}, "verdict": "ft_faster",
    }


class ReceiptValidation(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.receipt = valid_receipt(self.root)

    def tearDown(self):
        self.tmp.cleanup()

    def errors(self):
        return h2h.validate_receipt(self.receipt, self.root)

    def test_a_valid_receipt_passes(self):
        self.assertEqual(self.errors(), [])

    def test_a_refused_row_must_not_report_a_ratio(self):
        self.receipt["rows"][0]["verdict"] = self.receipt["verdict"] = "NO_ADMISSIBLE_RATIO (load: high)"
        self.assertEqual(self.errors(), ["rows[0] refused a verdict but still reports ft_speedup"])
        self.receipt["rows"][0]["ft_speedup"] = None
        self.assertEqual(self.errors(), [])

    def test_a_verdict_must_agree_with_its_ratio_and_the_primary_row(self):
        self.receipt["rows"][0]["ft_speedup"] = 0.5
        self.assertIn("rows[0].verdict contradicts ft_speedup 0.5", self.errors())
        self.receipt["rows"][0]["ft_speedup"] = 1.5
        self.receipt["verdict"] = "ghostty_faster"
        self.assertIn("verdict must equal the primary corpus row's verdict", self.errors())
        self.receipt["verdict"] = "mixed"
        self.assertTrue(any("is not ft_faster" in e for e in self.errors()))

    def test_the_primary_corpus_comes_first_and_carries_a_parity_check(self):
        extra = dict(self.receipt["rows"][0], corpus="seq_lines")
        self.receipt["rows"].insert(0, extra)
        self.assertIn("the primary corpus's row must come first", self.errors())
        self.receipt["rows"].pop(0)
        self.receipt["emoji_width_parity"] = []
        self.assertEqual(self.errors(), ["emoji width parity must be checked when the primary corpus is measured"])

    def test_identity_fields(self):
        self.receipt["ghostty"]["checkout_clean"] = False
        self.receipt["binaries"]["ghostty_probe"]["sha256"] = "short"
        self.receipt["rows"][0]["input_sha256"] = "e" * 64
        errors = self.errors()
        self.assertIn("ghostty.checkout_clean must be true", errors)
        self.assertIn("binaries.ghostty_probe.sha256 must be a SHA-256", errors)
        self.assertIn("rows[0].input_sha256 is not one of the inputs", errors)

    def test_retained_hyperfine_exports_must_exist(self):
        self.receipt["rows"][0]["hyperfine_json"].append("round-02.json")
        self.assertEqual(self.errors(), ["rows[0]: retained export rows/color_emoji_random@80x120/round-02.json is missing"])

    def test_samples_must_be_positive_and_counted(self):
        self.receipt["rows"][0]["arms"]["ghostty"]["times_s"] = [0.0, 0.5]
        self.assertIn("rows[0].arms.ghostty.times_s must be positive numbers", self.errors())
        self.receipt["rows"][0]["arms"]["ghostty"]["times_s"] = [0.5, 0.5, 0.5]
        self.assertIn("rows[0].arms.ghostty.n must equal len(times_s)", self.errors())
        self.receipt["rows"][0]["arms"]["ghostty"]["cv_pct"] = math.nan
        self.assertIn("rows[0].arms.ghostty.cv_pct must be a number", self.errors())


if __name__ == "__main__":
    unittest.main(verbosity=1)
