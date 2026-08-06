"""dependency-check — crate dependency direction & inventory placement (ENG-ARCH-001).

Two things it can check today without inventing new policy:
  1. spec/dependencies.yaml `allowed_layers` must name real crates (or `any-dev`) — a typo
     there silently disables the placement rule for a DEP.
  2. The actual internal crate graph (from `cargo metadata`) must be ACYCLIC. ARCH forbids
     crate cycles regardless of any declared layering, so cycle detection is always correct.

If spec/dependencies.yaml grows an explicit `crate_layers:` map (crate -> allowed crate
deps), this also enforces direction against it; until then that check reports "not declared"
rather than guessing a layering from prose.
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from rusekit import repo, render  # noqa: E402

try:
    import yaml
except ImportError:  # pragma: no cover
    yaml = None


def _cargo_metadata() -> dict | None:
    try:
        p = subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--no-deps", "-q"],
            cwd=repo.ROOT, capture_output=True, text=True, timeout=120)
    except (FileNotFoundError, subprocess.SubprocessError):
        return None
    if p.returncode != 0:
        return None
    try:
        return json.loads(p.stdout)
    except json.JSONDecodeError:
        return None


def _internal_graph(meta: dict) -> dict[str, list[str]]:
    members = {pkg["name"] for pkg in meta.get("packages", [])}
    graph: dict[str, list[str]] = {}
    for pkg in meta.get("packages", []):
        deps = [d["name"] for d in pkg.get("dependencies", []) if d["name"] in members]
        graph[pkg["name"]] = sorted(set(deps))
    return graph


def _closure(crates: dict) -> dict:
    """crate -> set of all TRANSITIVELY allowed crate deps, from architecture.yaml may_depend_on."""
    direct = {c: set((v or {}).get("may_depend_on") or []) for c, v in crates.items()}
    out: dict = {}
    for c in direct:
        seen: set = set()
        stack = list(direct[c])
        while stack:
            n = stack.pop()
            if n in seen:
                continue
            seen.add(n)
            stack.extend(direct.get(n, set()))
        out[c] = seen
    return out


def _find_cycle(graph: dict[str, list[str]]) -> list[str] | None:
    WHITE, GRAY, BLACK = 0, 1, 2
    color = {n: WHITE for n in graph}
    stack: list[str] = []

    def dfs(n: str) -> list[str] | None:
        color[n] = GRAY
        stack.append(n)
        for m in graph.get(n, []):
            if color.get(m, WHITE) == GRAY:
                return stack[stack.index(m):] + [m]
            if color.get(m, WHITE) == WHITE:
                r = dfs(m)
                if r:
                    return r
        color[n] = BLACK
        stack.pop()
        return None

    for n in graph:
        if color[n] == WHITE:
            r = dfs(n)
            if r:
                return r
    return None


def main(argv: list[str]) -> int:
    argparse.ArgumentParser(prog="ruse arch deps").parse_args(argv)
    render.heading("dependency-check")
    errors: list[str] = []
    warns: list[str] = []

    # 1. allowed_layers reference real crates
    if yaml is not None and os.path.isfile(repo.path("spec/dependencies.yaml")):
        deps = yaml.safe_load(open(repo.path("spec/dependencies.yaml"))) or {}
        valid = set(repo.CRATES) | {"any-dev"}
        for did, d in (deps.get("dependencies") or {}).items():
            for layer in d.get("allowed_layers") or []:
                if layer not in valid:
                    errors.append(f"{did}: allowed_layers '{layer}' is not a real crate "
                                  f"(known: {sorted(valid)})")
    else:
        warns.append("spec/dependencies.yaml not readable — skipped allowed_layers check")

    # 1b. Architecture-as-Code: the crate dependency contract (spec/architecture.yaml, ARCH-LAYER-001).
    arch: dict = {}
    if yaml is not None and os.path.isfile(repo.path("spec/architecture.yaml")):
        arch = yaml.safe_load(open(repo.path("spec/architecture.yaml"))) or {}
        for c in (arch.get("crates") or {}):
            if c not in repo.CRATES:
                errors.append(f"architecture.yaml: crate '{c}' is not a real crate {sorted(repo.CRATES)}")
    else:
        warns.append("spec/architecture.yaml not found — no crate dependency contract (ARCH-LAYER-001)")
    allowed_closure = _closure(arch.get("crates") or {})

    # 2. internal crate graph: cycles + optional direction
    meta = _cargo_metadata()
    if meta is None:
        warns.append("cargo metadata unavailable — skipped crate-graph checks")
    else:
        graph = _internal_graph(meta)
        edges = sum(len(v) for v in graph.values())
        render.field("Crates", str(len(graph)))
        render.field("Internal edges", str(edges))
        for src in sorted(graph):
            if graph[src]:
                render.bullet(f"{src} → {', '.join(graph[src])}")
        cyc = _find_cycle(graph)
        if cyc:
            errors.append("crate dependency cycle: " + " → ".join(cyc))
        # crate dependency contract: cargo edges ⊆ transitive may_depend_on (ARCH-LAYER-001)
        if allowed_closure:
            for src in sorted(graph):
                for dst in graph.get(src, []):
                    if dst not in allowed_closure.get(src, set()):
                        allowed = sorted(allowed_closure.get(src, set())) or ["nothing"]
                        errors.append(f"forbidden crate dep: {src} → {dst} "
                                      f"(architecture.yaml allows {src} → {allowed})")
            render.bullet(f"crate contract: {len(allowed_closure)} crates, direction enforced "
                          "(ARCH-LAYER-001)", mark="·")
        else:
            render.bullet("no architecture.yaml crate contract — cycles enforced, direction skipped",
                          mark="·")
        # forbidden module edges: declared now, source-enforceable once crates have code
        fme = arch.get("forbidden_module_edges") or []
        if fme:
            render.bullet(f"forbidden module edges: {len(fme)} declared "
                          "(source-scan NOT-YET — needs code)", mark="·")

    print()
    for w in warns:
        render.warn(w)
    for e in errors:
        render.fail(e)
    if errors:
        render.fail("dependency-check: FAIL")
        return 1
    render.ok("dependency-check: PASS")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
