#!/usr/bin/env python3
"""gov context_digest — the hand-maintained digest must cite every decided D-* and every INV-*.

spec/CONTEXT.md is a hand-written digest until the generator (D-022, #982) replaces it, and it went stale twice
in one milestone (#1332, #1360): decisions and invariants landed in their registries and the digest never named
them. This gate holds only the narrowest fact that would have caught that — the digest stays a summary, not a
copy:

  R1  every `## D-<n>` heading in spec/DECISIONS.md whose status begins with `decided` MUST appear in
      spec/CONTEXT.md as the token `D-<n>`. Open and superseded decisions are not required.
  R2  every invariant the registry defines (`- **INV-...**` in docs/invariants/reference-invariants.md) MUST
      appear in spec/CONTEXT.md as that exact token. A glob such as `INV-PLUGIN-*` does not cite an ID.

FAILS on any missing ID. Retire it when the generator lands (#982). Auto-discovered into `ruse gov check`.
"""
from __future__ import annotations

import argparse
import re
import sys

from rusekit import render, repo  # noqa: E402

DIGEST = "spec/CONTEXT.md"
DECISIONS = "spec/DECISIONS.md"
INVARIANTS = "docs/invariants/reference-invariants.md"

# `## D-065 — title · decided (design)`: the status is the segment after the title's first ` · `.
DHEAD = re.compile(r"^## (D-\d+) — .*? · (.*)$", re.M)
IDEF = re.compile(r"^- \*\*(INV-[A-Z0-9]+(?:-[A-Z0-9]+)*)\*\*", re.M)
TOKEN = re.compile(r"(?<![A-Za-z0-9-])((?:D|INV)-[A-Z0-9]+(?:-[A-Z0-9]+)*)(?![A-Za-z0-9-])")


def decided_decisions(text: str) -> list[str]:
    return [did for did, status in DHEAD.findall(text) if status.strip().startswith("decided")]


def defined_invariants(text: str) -> list[str]:
    return IDEF.findall(text)


def cited(text: str) -> set[str]:
    return set(TOKEN.findall(text))


def missing(digest: str, decisions: str, invariants: str) -> tuple[list[str], list[str]]:
    have = cited(digest)
    return ([d for d in decided_decisions(decisions) if d not in have],
            [i for i in defined_invariants(invariants) if i not in have])


def _read(rel: str) -> str:
    with open(repo.path(rel), encoding="utf-8") as fh:
        return fh.read()


def main(argv=None) -> int:
    argparse.ArgumentParser(prog="gov context_digest").parse_args(argv or [])
    decisions, invariants = _read(DECISIONS), _read(INVARIANTS)
    miss_d, miss_i = missing(_read(DIGEST), decisions, invariants)

    render.heading("gov context_digest (CONTEXT.md cites every decided D-* and every INV-*)")
    render.field("Decided decisions", str(len(decided_decisions(decisions))))
    render.field("Invariants", str(len(defined_invariants(invariants))))
    for did in miss_d:
        render.bullet(f"{DIGEST}: {did} is decided in {DECISIONS} but the digest never cites it", mark="!")
    for iid in miss_i:
        render.bullet(f"{DIGEST}: {iid} is defined in {INVARIANTS} but the digest never cites it", mark="!")

    if miss_d or miss_i:
        render.fail(f"{len(miss_d)} decision(s) and {len(miss_i)} invariant(s) missing from {DIGEST}")
        return 1
    render.ok(f"{DIGEST} cites every decided D-* and every INV-*")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
