"""Per-thread busy/blocked split and top self-time frames from macOS `sample` output.

usage: python3 -I analyze.py SAMPLE.txt [THREAD_SUBSTRING ...]
"""
import re
import sys
from collections import Counter, defaultdict

WAIT = re.compile(
    r"__psynch_cvwait|__psynch_mutexwait|__psynch_rw_|mach_msg2_trap|mach_msg_trap|"
    r"__semwait_signal|kevent|__select|__ulock_wait|__workq_kernreturn|\bpoll\b|"
    r"__poll|nanosleep|__sigsuspend|__wait4|semaphore_wait|semaphore_timedwait|"
    r"__read_nocancel|^read\b|swtch_pri|thread_switch|"
    r"__accept|__recvfrom|__connect"
)
LINE = re.compile(r"^(?P<prefix>[ +!:|]*?)(?P<count>\d+) (?P<frame>.*)$")


def rust_v0_path(symbol):
    """The identifiers of a Rust v0 symbol, in order (enough to read a path)."""
    idents = []
    i = 0
    while i < len(symbol):
        if symbol[i].isdigit() and (i == 0 or not symbol[i - 1].isdigit()):
            j = i
            while j < len(symbol) and symbol[j].isdigit():
                j += 1
            n = int(symbol[i:j])
            start = j + 1 if j < len(symbol) and symbol[j] == "_" else j
            ident = symbol[start:start + n]
            if n > 0 and len(ident) == n and re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", ident):
                idents.append(ident)
                i = start + n
                continue
        i += 1
    return idents


def frame_name(raw):
    raw = re.sub(r"\s+\[0x[0-9a-f,.x]+\]\s*$", "", raw)
    raw = re.sub(r"\s+\+\s+[\d,.]+\s*$", "", raw)
    raw = raw.strip()
    m = re.match(r"^(_R\S+)\s+\(in [^)]*\)", raw)
    if m:
        idents = rust_v0_path(m.group(1))
        if idents:
            return "::".join(idents[-4:])
    m = re.match(r"^(\S+)\s+\(in ([^)]*)\)", raw)
    if m:
        return f"{m.group(1)} [{m.group(2)}]"
    return raw


def parse(path):
    threads = {}
    current = None
    stack = []  # (depth, node)
    in_graph = False
    for line in open(path, errors="replace"):
        line = line.rstrip("\n")
        if line.startswith("Call graph:"):
            in_graph = True
            continue
        if not in_graph:
            continue
        if line.startswith("Total number in stack"):
            break
        if not line.strip():
            continue
        body = line[4:] if line.startswith("    ") else line
        m = LINE.match(body)
        if not m:
            continue
        depth = len(m.group("prefix")) // 2
        count = int(m.group("count"))
        frame = m.group("frame")
        if depth == 0 and frame.startswith("Thread_"):
            name = frame.split(":", 1)[1].strip() if ":" in frame else frame
            current = {"name": name, "total": count, "children": [], "frame": "<thread>", "count": count}
            threads[frame] = current
            stack = [(-1, current)]
            continue
        node = {"frame": frame_name(frame), "count": count, "children": []}
        while stack and stack[-1][0] >= depth:
            stack.pop()
        stack[-1][1]["children"].append(node)
        stack.append((depth, node))
    return threads


def self_times(node, acc, path_wait):
    child_sum = sum(c["count"] for c in node["children"])
    own = node["count"] - child_sum
    if own > 0 and node["frame"] != "<thread>":
        acc[node["frame"]] += own
    for c in node["children"]:
        self_times(c, acc, path_wait)


def main():
    path = sys.argv[1]
    wanted = sys.argv[2:]
    threads = parse(path)
    rows = []
    for key, t in threads.items():
        acc = Counter()
        self_times(t, acc, False)
        total = t["total"]
        wait = sum(v for k, v in acc.items() if WAIT.search(k))
        rows.append((total - wait, total, t["name"], acc, wait))
    rows.sort(key=lambda r: -r[0])
    print(f"{'busy':>6} {'total':>6}  thread")
    for busy, total, name, acc, wait in rows[:14]:
        print(f"{busy:6d} {total:6d}  {name}  ({100*busy/max(total,1):.0f}% on CPU)")
    for busy, total, name, acc, wait in rows:
        if wanted and not any(w in name for w in wanted):
            continue
        if not wanted and busy < total * 0.05:
            continue
        print(f"\n== {name}: busy {busy}/{total} samples")
        for frame, n in acc.most_common(40):
            if WAIT.search(frame):
                continue
            print(f"  {n:6d} {100*n/max(busy,1):5.1f}%  {frame[:150]}")
        waits = [(f, n) for f, n in acc.most_common() if WAIT.search(f)]
        if waits:
            print("  waiting in: " + "; ".join(f"{n} {f[:60]}" for f, n in waits[:4]))


main()
