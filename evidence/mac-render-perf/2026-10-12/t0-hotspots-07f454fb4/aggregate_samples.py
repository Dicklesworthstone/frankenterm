"""Aggregate several `sample` files: per-thread on-CPU share and top self frames.

usage: python3 -I aggregate_samples.py OUT.md SAMPLE.txt...
(gunzip the sample-run*.txt.gz files first)
"""
import importlib.util
import os
import sys
from collections import Counter, defaultdict

spec = importlib.util.spec_from_file_location(
    "analyze", os.path.join(os.path.dirname(os.path.abspath(__file__)), "analyze_sample.py")
)
analyze = importlib.util.module_from_spec(spec)
sys.argv, saved = [sys.argv[0], "/dev/null"], sys.argv
spec.loader.exec_module(analyze)  # runs main() on /dev/null: harmless
sys.argv = saved

THREADS = [
    ("main", "main"),
    ("scrollback-durability", "scrollback-durability"),
    ("mux-parse-pane-", "mux-parse-pane-0"),
    ("mux-read-pane-", "mux-read-pane-0"),
]


def thread_key(name):
    for prefix, label in THREADS:
        if name.startswith(prefix):
            return label
    return None


def main():
    out, files = sys.argv[1], sys.argv[2:]
    busy = Counter()
    total = Counter()
    frames = defaultdict(Counter)
    waits = defaultdict(Counter)
    for path in files:
        for t in analyze.parse(path).values():
            label = thread_key(t["name"])
            if label is None:
                continue
            acc = Counter()
            analyze.self_times(t, acc, False)
            total[label] += t["total"]
            for frame, n in acc.items():
                if analyze.WAIT.search(frame):
                    waits[label][frame] += n
                else:
                    busy[label] += n
                    frames[label][frame] += n
    lines = []
    lines.append(f"Samples: {len(files)} files, 1 ms interval.\n")
    lines.append("| Thread | On CPU | Samples |")
    lines.append("|---|---|---|")
    for _, label in THREADS:
        if total[label]:
            lines.append(f"| {label} | {100 * busy[label] / total[label]:.0f}% | {total[label]} |")
    for _, label in THREADS:
        if not total[label]:
            continue
        lines.append(f"\n### {label}: top self frames (share of its on-CPU samples)\n")
        lines.append("| Share | Frame |")
        lines.append("|---|---|")
        for frame, n in frames[label].most_common(25):
            lines.append(f"| {100 * n / max(busy[label], 1):.1f}% | `{frame[:110]}` |")
        top_waits = waits[label].most_common(3)
        if top_waits:
            share = ", ".join(f"`{f[:40]}` {100 * n / total[label]:.0f}%" for f, n in top_waits)
            lines.append(f"\nBlocked in: {share} of all its samples.")
    open(out, "w").write("\n".join(lines) + "\n")
    print("\n".join(lines))


main()
