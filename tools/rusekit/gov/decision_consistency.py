#!/usr/bin/env python3
"""gov decision_consistency — decision references must resolve, and tension records must stay coherent.

Forward-references to decisions accumulate faster than the decisions get written: the D-047 amendment left a
concept still `blocks:`-ing a component the decision had just unblocked, and nothing caught it. This gate makes
three invariants executable:

  R1  every NUMERIC decision ref `D-<n>` anywhere under spec/ MUST resolve to a `## D-<n>` heading in
      spec/DECISIONS.md. Symbolic placeholders like `D-EDITLANG-PRIMITIVE` are deliberately non-numeric and
      are allowed — they mark an unopened FUTURE decision, not a dangling one.
  R2  an irreconcilable concept with `resolution: unified` MUST name the `kernel_concept` that unifies it
      (the executable form of irreconcilable.yaml's own LLM rule).
  R3  an irreconcilable concept with `resolution: pending` MUST declare a non-empty `blocks:` — a pending
      tension that blocks nothing is either already resolved or mis-marked.

FAILS on any dangling numeric ref, a unified concept with no kernel_concept, or a pending concept that blocks
nothing. Auto-discovered into `ruse gov check`.
"""
from __future__ import annotations

import argparse
import glob
import re
import sys

import yaml

from rusekit import render, repo  # noqa: E402

DECISIONS = "spec/DECISIONS.md"
IRRECON = "spec/parity/concepts/irreconcilable.yaml"
SPEC_GLOBS = ("spec/**/*.yaml", "spec/**/*.md")
DREF = re.compile(r"\bD-\d+\b")
DDEF = re.compile(r"^## (D-\d+)", re.M)


def defined_decisions() -> set[str]:
    with open(repo.path(DECISIONS), encoding="utf-8") as fh:
        return set(DDEF.findall(fh.read()))


def dangling_refs(defined: set[str]) -> list[tuple[str, str]]:
    out, seen = [], set()
    for pat in SPEC_GLOBS:
        for path in glob.glob(repo.path(pat), recursive=True):
            try:
                txt = open(path, encoding="utf-8").read()
            except (UnicodeDecodeError, IsADirectoryError, FileNotFoundError):
                continue
            for ref in DREF.findall(txt):
                key = (ref, path)
                if ref not in defined and key not in seen:
                    seen.add(key)
                    out.append((ref, repo.rel(path)))
    return out


def concept_issues() -> tuple[list[str], list[str]]:
    doc = yaml.safe_load(open(repo.path(IRRECON), encoding="utf-8")) or {}
    unified_no_kernel, pending_no_blocks = [], []
    for cid, body in (doc.get("concepts") or {}).items():
        body = body or {}
        res = body.get("resolution")
        if res == "unified" and not body.get("kernel_concept"):
            unified_no_kernel.append(cid)
        if res == "pending" and not body.get("blocks"):
            pending_no_blocks.append(cid)
    return unified_no_kernel, pending_no_blocks


def main(argv=None) -> int:
    argparse.ArgumentParser(prog="gov decision_consistency").parse_args(argv or [])
    defined = defined_decisions()
    dangling = dangling_refs(defined)
    unified_no_kernel, pending_no_blocks = concept_issues()

    render.heading("gov decision_consistency (decision refs + tension coherence)")
    render.field("Decisions defined", str(len(defined)))
    render.field("Dangling numeric refs", str(len(dangling)))

    for ref, rel in dangling:
        render.bullet(f"{rel}: cites {ref} but no `## {ref}` exists in DECISIONS.md — open it, fix the "
                      f"number, or use a symbolic D-NAME for an unopened decision", mark="!")
    for cid in unified_no_kernel:
        render.bullet(f"{cid}: resolution=unified but names no kernel_concept", mark="!")
    for cid in pending_no_blocks:
        render.bullet(f"{cid}: resolution=pending but blocks nothing — resolve it or state what it blocks",
                      mark="!")

    if dangling or unified_no_kernel or pending_no_blocks:
        render.fail(f"{len(dangling)} dangling ref(s), {len(unified_no_kernel)} unified-without-kernel, "
                    f"{len(pending_no_blocks)} pending-without-blocks")
        return 1
    render.ok("every D-<n> ref resolves; every unified concept names its kernel; every pending concept "
              "blocks something")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
