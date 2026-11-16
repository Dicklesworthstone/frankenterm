"""Callers of one function in one thread of a macOS `sample` call graph: sample counts per caller
chain (up to DEPTH frames above the target), demangled with rustfilt.

    python3 -I sample_callers.py <sample.txt> <thread-name substring> <function substring> [depth] [top N]

The thread-name substring matches the whole thread header line. The function substring matches
the demangled name as sample_agg.py shortens it (generic arguments dropped, a method's qualified
self kept). A stack counts once, at its outermost match.
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
    """As in sample_agg.py, cut to 110 characters."""
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
                    return (qualified + strip_generics(name[index + 1 :]))[:110]
    return strip_generics(name)[:110]


def main():
    path, want, target = sys.argv[1], sys.argv[2], sys.argv[3]
    depth_up = int(sys.argv[4]) if len(sys.argv) > 4 else 3
    top = int(sys.argv[5]) if len(sys.argv) > 5 else 15
    lines = open(path, encoding="utf-8", errors="replace").read().split("\n")
    frames, inside = [], False
    for line in lines:
        header = THREAD.match(line)
        if header:
            inside = want in header.group(0)
            continue
        if line.startswith("Total number in stack") or line.startswith("Binary Images"):
            inside = False
        if not inside:
            continue
        frame = FRAME.match(line)
        if frame:
            frames.append((len(frame.group(1)), int(frame.group(2)), frame.group(3)))
    names = demangle(sorted({f[2] for f in frames}))
    chains, stack, total = Counter(), [], 0
    for depth, count, raw in frames:
        while stack and stack[-1][0] >= depth:
            stack.pop()
        name = short(names.get(raw, raw))
        if target in name and all(target not in entry[1] for entry in stack):
            chain = " <- ".join(entry[1] for entry in reversed(stack[-depth_up:]))
            chains[chain] += count
            total += count
        stack.append((depth, name))
    print(f"'{target}' in '{want}': {total} samples")
    for chain, count in chains.most_common(top):
        print(f"{count:7d}  {chain}")


main()
