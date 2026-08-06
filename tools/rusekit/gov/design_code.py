#!/usr/bin/env python3
"""gov design_code — enforce that code in design docs is NON-NORMATIVE (D-038).

Governance policy (D-038): a design doc's job is the CONTRACT — invariants, field semantics, algorithms —
not the concrete type. Concrete types have one authoritative home: code (internal types), or spec/contracts/
(cross-boundary formats/protocols). A hand-written struct in prose is a drift liability: when the code or
design changes, nobody chases the copies. So any design doc that shows code MUST mark it illustrative with
the banner below, which points readers at the real source.

This checker surfaces the gap; it does not yet FAIL the build (warn-only, `[partial]`). Once code exists the
successor check is doc<->code (extract the block, diff against the real type) — until then the banner keeps a
sketch from reading as authoritative. Portable engine; auto-discovered into `ruse gov check`.
"""
from __future__ import annotations

import argparse
import glob
import os
import re
import sys

from rusekit import repo, render  # noqa: E402

# Language-tagged fences that are actual code (not ascii diagrams / shell transcripts we don't govern).
CODE_FENCE = re.compile(r"^```(rust|python|toml|json|ts|typescript|javascript)\b", re.M)
BANNER = "code-blocks: illustrative"   # the marker a design doc adds once (near its code or in frontmatter)
ROOTS = ("docs/design", "docs/rfc")


def scan() -> list[dict]:
    out = []
    for root in ROOTS:
        for p in sorted(glob.glob(repo.path(root, "**", "*.md"), recursive=True)):
            text = open(p, encoding="utf-8").read()
            if not CODE_FENCE.search(text):
                continue
            out.append({"rel": repo.rel(p), "banner": BANNER in text,
                        "blocks": len(CODE_FENCE.findall(text))})
    return out


def main(argv=None) -> int:
    argparse.ArgumentParser(prog="gov design_code").parse_args(argv or [])
    docs = scan()
    missing = [d for d in docs if not d["banner"]]

    render.heading("gov design_code (D-038: design-doc code is non-normative)")
    render.field("Design/RFC docs with code", str(len(docs)))
    render.field("Marked illustrative", str(len(docs) - len(missing)))
    render.field("Missing banner", str(len(missing)))
    for d in missing:
        render.bullet(f"{d['rel']}  ({d['blocks']} code block(s)) — add `{BANNER}` banner", mark="!")
    if missing:
        # warn-only for now (policy is declared + surfaced, not yet blocking) — never silently pass.
        render.warn(f"{len(missing)} design doc(s) show code without the illustrative banner (D-038, warn)")
    else:
        render.ok("all design-doc code is marked illustrative (D-038)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
