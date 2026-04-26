#!/usr/bin/env python3
"""ft-94juo — workspace cycle detector.

Reads `cargo metadata --format-version=1 --no-deps` JSON from stdin,
builds an adjacency list scoped to workspace members (skipping crates.io
and git deps), and runs a DFS cycle detector. On hit: prints every edge
in the cycle and exits 1. On clean: prints a one-line summary and exits 0.

Skips dev-dep edges in the cycle detector to match cargo's own
forgiveness — dev cycles are allowed (they only matter when a crate's
tests need the back-edge crate, which cargo handles by building twice).
"""

from __future__ import annotations

import json
import sys


def main() -> int:
    meta = json.loads(sys.stdin.read())

    # `cargo metadata --no-deps` returns packages = workspace members
    # only (transitive deps are suppressed). So every package.name in
    # the response IS a workspace member; collect them as the graph
    # node set.
    #
    # (Note: the `workspace_members` field switched format around cargo
    # 1.78 from "name version (url)" to "path+file:///path#version".
    # Going via packages[].name avoids that fragility entirely.)
    ws_names: set[str] = {pkg["name"] for pkg in meta.get("packages", [])}

    # Build adjacency list: pkg.name -> [(dep.name, kind)]
    # `kind` is one of: "normal" / "build" / "dev" (cargo terminology).
    graph: dict[str, list[tuple[str, str]]] = {}
    for pkg in meta.get("packages", []):
        name = pkg["name"]
        if name not in ws_names:
            continue
        edges: list[tuple[str, str]] = []
        for dep in pkg.get("dependencies", []):
            dep_name = dep.get("name")
            if dep_name in ws_names and dep_name != name:
                kind = dep.get("kind") or "normal"
                edges.append((dep_name, kind))
        graph[name] = edges

    # DFS cycle detection.
    WHITE, GRAY, BLACK = 0, 1, 2
    color: dict[str, int] = {n: WHITE for n in graph}
    parent: dict[str, str | None] = {n: None for n in graph}
    edge_kind: dict[tuple[str, str], str] = {}
    cycles: list[list[str]] = []

    def dfs(start: str) -> None:
        stack: list[tuple[str, object]] = [(start, iter(graph[start]))]
        color[start] = GRAY
        while stack:
            node, it = stack[-1]
            try:
                nxt, kind = next(it)  # type: ignore[arg-type, call-overload]
            except StopIteration:
                color[node] = BLACK
                stack.pop()
                continue
            # Dev-dep cycles are ALLOWED by cargo. Skip them in the
            # cycle detector to match cargo's own forgiveness.
            if kind == "dev":
                continue
            edge_kind[(node, nxt)] = kind
            if color[nxt] == WHITE:
                parent[nxt] = node
                color[nxt] = GRAY
                stack.append((nxt, iter(graph[nxt])))
            elif color[nxt] == GRAY:
                # Back-edge → cycle. Walk parent pointers from `node`
                # until we return to `nxt`.
                cycle = [nxt]
                cur: str | None = node
                while cur is not None and cur != nxt:
                    cycle.append(cur)
                    cur = parent[cur]
                cycle.append(nxt)
                cycle.reverse()
                cycles.append(cycle)

    for n in graph:
        if color[n] == WHITE:
            dfs(n)

    if not cycles:
        prod_edges = sum(
            sum(1 for _, k in es if k != "dev") for es in graph.values()
        )
        print(
            f"ft-94juo guard: workspace dep graph is acyclic. "
            f"{len(graph)} workspace members checked, "
            f"{prod_edges} intra-workspace prod/build edges."
        )
        return 0

    # Cycle(s) found — print every edge with kind annotation.
    print(f"ft-94juo guard: workspace dep graph has {len(cycles)} cycle(s)!")
    print()
    for i, cyc in enumerate(cycles, 1):
        print(f"Cycle {i}:")
        for a, b in zip(cyc, cyc[1:]):
            kind = edge_kind.get((a, b), "?")
            arrow = "──→" if kind == "normal" else f"──[{kind}]→"
            print(f"  {a:40s} {arrow}  {b}")
        print()
    print("Hint: cargo cycles caught at this layer almost always come from a")
    print("sub-crate adding `frankenterm-core` as a path-dep while also being")
    print("re-exported from `frankenterm-core/src/lib.rs`. The fix is usually")
    print("to either (a) push the shared types DOWN into a new tier-1 leaf")
    print("crate (see ft-usvnt → resource-types as the canonical pattern), or")
    print("(b) make the sub-crate a `pub use` consumer rather than a back-")
    print("reference. See docs/proposals/ft-l3tfo-cold-build-measurements.md")
    print("and docs/proposals/ft-t2d70-mcp-connector-extraction-feasibility.md")
    print("for prior cycle-resolution case studies.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
