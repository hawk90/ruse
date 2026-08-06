"""change impact — what does this change ripple into?

Two entry points, one graph (rusekit/model.py):
  --from <ID>     start from a spec ID (CAP-*, F-*, C-*, D-*, RFC-*, DEP-*, INV-*, ARCH-*)
  --changed/--files/--base   start from the IDs that own the changed files

It prints the direct impact (the roots), a grouped reachable set, and an edge tree so a
human reviewer can see *why* each node is implicated. `--json`/`--out` persist it for
`.ruse/work/<id>/impact.json` and PR rendering.
"""
from __future__ import annotations

import argparse
import json
import os
import sys

from rusekit import repo, render, model as model_mod  # noqa: E402

KIND_GROUP = {
    "CAP": "Capabilities", "F": "Requirements", "C": "Requirements",
    "INV": "Invariants", "DEP": "Dependencies", "D": "Decisions",
    "ARCH": "Architecture", "RFC": "RFCs", "CRATE": "Crates", "ENG": "Principles",
    "DOC": "Docs",
}
GROUP_ORDER = ["Requirements", "Capabilities", "Invariants", "Principles",
               "Decisions", "Architecture", "RFCs", "Dependencies", "Crates", "Docs"]


def roots_from_files(m: model_mod.Model, files: list[str]) -> tuple[list[str], list[str]]:
    """(root ids, files that mapped to nothing)."""
    roots: set[str] = set()
    unmapped: list[str] = []
    for f in files:
        ids = m.ids_for_file(f)
        if ids:
            roots |= ids
        else:
            unmapped.append(f)
    return sorted(roots), unmapped


def bfs_tree(m: model_mod.Model, roots: list[str], direction: str, depth: int,
             cap: int = 250):
    """BFS that records a spanning tree: returns (order, parent, via, dist)."""
    dist = {r: 0 for r in roots if m.has(r)}
    parent: dict[str, str | None] = {r: None for r in dist}
    via: dict[str, str] = {r: "" for r in dist}
    order = list(dist)
    frontier = list(dist)
    d = 0
    truncated = False
    while frontier and d < depth:
        d += 1
        nxt = []
        for nid in frontier:
            for e in m.neighbors(nid, direction):
                other = e.dst if e.src == nid else e.src
                if other in m.nodes and other not in dist:
                    if len(order) >= cap:
                        truncated = True
                        break
                    dist[other] = d
                    parent[other] = nid
                    via[other] = ("→" if e.src == nid else "←") + e.rel
                    order.append(other)
                    nxt.append(other)
            if truncated:
                break
        frontier = nxt
        if truncated:
            break
    return order, parent, via, dist, truncated


def _label(m: model_mod.Model, nid: str) -> str:
    t = m.title(nid)
    return f"{nid}  {t}".rstrip() if t else nid


def run(roots: list[str], direction: str, depth: int, m: model_mod.Model) -> dict:
    order, parent, via, dist, truncated = bfs_tree(m, roots, direction, depth)
    reachable = [n for n in order if n not in roots]

    render.heading("change impact")
    render.field("Roots", str(len(roots)))
    render.field("Direction", direction)
    render.field("Depth", str(depth))

    render.heading("\nDirect impact")
    for r in roots:
        n = m.nodes.get(r)
        kind = f"[{n.kind}] " if n else ""
        render.bullet(f"{kind}{_label(m, r)}")

    # grouped reachable
    groups: dict[str, list[str]] = {}
    for nid in reachable:
        k = m.nodes[nid].kind
        groups.setdefault(KIND_GROUP.get(k, k), []).append(nid)
    if reachable:
        render.heading(f"\nTransitive impact ({len(reachable)} nodes ≤{depth} hops)")
        for g in GROUP_ORDER:
            if g in groups:
                render.bullet(f"{g}: " + ", ".join(sorted(groups[g])))
    else:
        render.bullet("(nothing else reachable)")

    # edge tree (why)
    render.heading("\nWhy (edge tree)")
    lines: list[tuple[int, str]] = []
    for nid in order:
        d = dist[nid]
        rel = f" {via[nid]}" if via.get(nid) else ""
        lines.append((d, f"{nid}{rel}  {m.title(nid)}".rstrip()))
    render.tree(lines)
    if truncated:
        render.warn(f"tree truncated at {len(order)} nodes (raise --depth cap if needed)")

    return {
        "roots": roots,
        "direction": direction,
        "depth": depth,
        "reachable": {nid: {"kind": m.nodes[nid].kind, "title": m.title(nid),
                            "dist": dist[nid]} for nid in reachable},
        "truncated": truncated,
    }


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(prog="ruse impact")
    ap.add_argument("--from", dest="frm", nargs="*", help="spec ID(s) to start from")
    ap.add_argument("--changed", action="store_true", help="start from changed files")
    ap.add_argument("--files", nargs="*", help="explicit files (implies --changed)")
    ap.add_argument("--base", help="git ref to diff against")
    ap.add_argument("--issue", help="use this change.yaml's affected ids/paths")
    ap.add_argument("--direction", choices=["out", "in", "both"], default="both")
    ap.add_argument("--depth", type=int, default=3)
    ap.add_argument("--json", action="store_true", help="print JSON instead of a report")
    ap.add_argument("--out", help="also write JSON to this path")
    args = ap.parse_args(argv)

    m = model_mod.load()
    for e in m.errors:
        render.warn(e)

    roots: list[str] = []
    unmapped: list[str] = []
    if args.frm:
        roots = [r for r in args.frm if m.has(r)]
        missing = [r for r in args.frm if not m.has(r)]
        for r in missing:
            render.warn(f"unknown id: {r}")
    else:
        issue = args.issue or (repo.active_issue() if not (args.changed or args.files or args.base) else None)
        if issue:
            from rusekit import contract
            c = contract.load(issue) or {}
            ids = contract.affected_ids(c)
            roots = [r for r in ids if m.has(r)]
            if not roots:
                cs = repo.resolve_changeset(files=contract.declared_paths(c))
                roots, unmapped = roots_from_files(m, cs.files)
        else:
            cs = repo.resolve_changeset(base=args.base, files=args.files)
            if cs.source == "none":
                render.warn("no --from, no git, no --files: nothing to analyze.")
                return 1
            roots, unmapped = roots_from_files(m, cs.files)

    if not roots:
        render.fail("no spec IDs resolved from the given input")
        if unmapped:
            render.heading("Unmapped files (no owning spec ID)")
            for f in unmapped:
                render.bullet(f)
        return 1

    result = run(roots, args.direction, args.depth, m)
    if unmapped:
        render.heading("\nUnmapped files (touched but no owning spec ID)")
        for f in unmapped:
            render.bullet(f)
        result["unmapped_files"] = unmapped

    if args.out:
        os.makedirs(os.path.dirname(repo.path(args.out)) or ".", exist_ok=True)
        with open(repo.path(args.out), "w", encoding="utf-8") as fh:
            json.dump(result, fh, indent=2)
        render.field("\nWrote", args.out)
    if args.json:
        print(json.dumps(result, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
