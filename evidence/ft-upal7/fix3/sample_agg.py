"""Aggregate one thread of a macOS `sample` call graph: inclusive and self sample counts per
function, demangled with rustfilt.

    python3 -I sample_agg.py <sample.txt> <thread-name substring> [top N]

The thread-name substring matches the whole thread header line, so `Thread_<id>` selects one
thread; frames of every matching thread are aggregated together. Inclusive counts a function once
per stack (recursion is not double counted). Self is a frame's count minus its children's.
"""

import re
import subprocess
import sys
from collections import Counter

FRAME = re.compile(r"^([\s+!:|]*)(\d+)\s+(.+?)\s+\(in ([^)]+)\)")
THREAD = re.compile(r"^\s{4}(\d+)\s+Thread_\d+(.*)$")


def demangle(names):
    try:
        out = subprocess.run(
            ["rustfilt"],
            input="\n".join(names),
            capture_output=True,
            text=True,
            check=True,
        ).stdout.split("\n")
        return dict(zip(names, out))
    except (OSError, subprocess.CalledProcessError) as error:
        print(f"rustfilt failed ({error}): names stay mangled", file=sys.stderr)
        return {name: name for name in names}


def strip_generics(name):
    depth, out = 0, []
    for ch in name:
        if ch == "<":
            depth += 1
        elif ch == ">":
            depth = max(depth - 1, 0)
        elif depth == 0:
            out.append(ch)
    return "".join(out)


def short(name):
    """One row per function: generic arguments and the trailing hash dropped, but the qualified
    self of a method kept (`<Vec<T> as Drop>::drop` -> `<Vec as Drop>::drop`, `<Mux>::run` ->
    `<Mux>::run`), so methods of different types never merge."""
    name = re.sub(r"::h[0-9a-f]{16}$", "", name)
    if name.startswith("<"):
        depth = 0
        for index, ch in enumerate(name):
            if ch == "<":
                depth += 1
            elif ch == ">":
                depth -= 1
                if depth == 0:
                    qualified = "<" + strip_generics(name[1:index]) + ">"
                    return (qualified + strip_generics(name[index + 1 :]))[:150]
    return strip_generics(name)[:150]


def main():
    path, want = sys.argv[1], sys.argv[2]
    top = int(sys.argv[3]) if len(sys.argv) > 3 else 40
    lines = open(path, encoding="utf-8", errors="replace").read().split("\n")
    frames, total, inside = [], 0, False
    for line in lines:
        header = THREAD.match(line)
        if header:
            inside = want in header.group(0)
            if inside:
                total += int(header.group(1))
            continue
        if line.startswith("Total number in stack") or line.startswith("Binary Images"):
            inside = False
        if not inside:
            continue
        frame = FRAME.match(line)
        if frame:
            frames.append((len(frame.group(1)), int(frame.group(2)), frame.group(3), frame.group(4)))
    names = demangle(sorted({f[2] for f in frames}))
    inclusive, self_counts = Counter(), Counter()
    stack = []  # (depth, count, name, child total)

    def pop_to(depth):
        while stack and stack[-1][0] >= depth:
            _, count, name, children = stack.pop()
            self_counts[name] += count - children

    for depth, count, raw, lib in frames:
        pop_to(depth)
        name = short(names.get(raw, raw)) + "  [" + lib + "]"
        if stack:
            parent = stack[-1]
            stack[-1] = (parent[0], parent[1], parent[2], parent[3] + count)
        if all(entry[2] != name for entry in stack):
            inclusive[name] += count
        stack.append((depth, count, name, 0))
    pop_to(-1)
    print(f"thread '{want}': {total} samples")
    print("-- inclusive")
    for name, count in inclusive.most_common(top):
        print(f"{count:7d} {100.0 * count / max(total, 1):5.1f}%  {name}")
    print("-- self")
    for name, count in self_counts.most_common(top // 2):
        print(f"{count:7d} {100.0 * count / max(total, 1):5.1f}%  {name}")


main()
