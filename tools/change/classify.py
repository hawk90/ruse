"""change classify — compute the minimum required change-kind from the actual diff.

The rule (docs/contributing/change-paths.md): the classifier may only RAISE the required
risk above the declared kind, never lower it. It hard-FAILS on two unambiguous things —
a declared kind below the observed floor, and any edit to a generated file — and otherwise
emits human-judgment notes rather than pretending to read intent.
"""
from __future__ import annotations

import argparse
import os
import re
import sys
from dataclasses import dataclass, field

# Allow running both as `python -m` and as a plain path via ruse.py.
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from rusekit import repo, render, contract  # noqa: E402


@dataclass
class Classification:
    declared_kind: str | None
    declared_risk: int | None
    observed_kind: str | None
    observed_risk: int
    reasons: list[tuple[str, str, str]] = field(default_factory=list)  # (file, kind, why)
    notes: list[str] = field(default_factory=list)
    generated_hits: list[str] = field(default_factory=list)
    n_files: int = 0
    source: str = "none"

    @property
    def ok(self) -> bool:
        if self.generated_hits:
            return False
        if self.declared_risk is None:
            return False
        return self.declared_risk >= self.observed_risk


def _head(relpath: str, n: int = 2048) -> str:
    p = repo.path(relpath)
    try:
        with open(p, encoding="utf-8", errors="replace") as fh:
            return fh.read(n)
    except OSError:
        return ""


def classify_changeset(files: list[str], declared_kind: str | None,
                       source: str = "explicit") -> Classification:
    policy = contract.load_kinds()
    kinds = policy.get("kinds") or {}
    triggers = policy.get("path_triggers") or []
    human = policy.get("human_judgment") or []
    marker = policy.get("generated_marker")

    def risk_of(kind: str) -> int:
        return (kinds.get(kind) or {}).get("risk", 0)

    observed_risk = 0
    observed_kind: str | None = None
    reasons: list[tuple[str, str, str]] = []
    notes: list[str] = []
    generated: list[str] = []

    for f in files:
        # generated-file guard
        if marker and os.path.isfile(repo.path(f)) and marker in _head(f):
            generated.append(f)
        # path triggers → risk floor
        best: tuple[int, str, str] | None = None
        for t in triggers:
            if re.search(t["pattern"], f):
                r = risk_of(t["min_kind"])
                if best is None or r > best[0]:
                    best = (r, t["min_kind"], t.get("reason", ""))
        if best:
            reasons.append((f, best[1], best[2]))
            if best[0] > observed_risk:
                observed_risk, observed_kind = best[0], best[1]
        # human-judgment notes
        for h in human:
            if re.search(h["pattern"], f):
                notes.append(f"{f}: {h['note']}")

    return Classification(
        declared_kind=declared_kind,
        declared_risk=(risk_of(declared_kind) if declared_kind in kinds else None),
        observed_kind=observed_kind,
        observed_risk=observed_risk,
        reasons=reasons,
        notes=notes,
        generated_hits=generated,
        n_files=len(files),
        source=source,
    )


def _report(cl: Classification) -> None:
    render.heading("change classify")
    dk = f"{cl.declared_kind} (risk {cl.declared_risk})" if cl.declared_kind else "—"
    ok_ = f"{cl.observed_kind} (risk {cl.observed_risk})" if cl.observed_kind else "none (risk 0)"
    render.field("Declared kind", dk)
    render.field("Observed minimum", ok_)
    render.field("Changed files", f"{cl.n_files}  (source: {cl.source})")
    if cl.reasons:
        render.heading("\nReasons")
        for f, kind, why in cl.reasons:
            render.bullet(f"{f}  →  {kind}  ({why})")
    if cl.notes:
        render.heading("\nHuman judgment")
        for n in cl.notes:
            render.bullet(n)
    if cl.generated_hits:
        render.heading("\nGenerated files edited (forbidden — regenerate instead)")
        for g in cl.generated_hits:
            render.bullet(g, mark="✗")
    print()
    if cl.generated_hits:
        render.fail("a generated file was hand-edited (ENG-DOC-001)")
    elif cl.declared_risk is None:
        render.fail(f"declare a kind: --kind <{ '|'.join(contract.kind_names()) }>")
    elif cl.declared_risk < cl.observed_risk:
        render.fail(f"declared '{cl.declared_kind}' is below the observed minimum "
                    f"'{cl.observed_kind}' — raise the kind or split the change")
    else:
        extra = ""
        if cl.observed_kind and cl.declared_risk > cl.observed_risk:
            extra = " (declared heavier than observed — that is allowed)"
        render.ok(f"declared kind satisfies the observed minimum{extra}")


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(prog="ruse change classify")
    ap.add_argument("--base", help="git ref to diff against (e.g. origin/main)")
    ap.add_argument("--files", nargs="*", help="explicit file list (overrides git)")
    ap.add_argument("--kind", help="declared kind (default: read active change.yaml)")
    ap.add_argument("--issue", help="issue whose change.yaml supplies the declared kind/files")
    args = ap.parse_args(argv)

    cs = repo.resolve_changeset(base=args.base, files=args.files)

    declared = args.kind
    issue = args.issue or repo.active_issue()
    if declared is None and issue:
        c = contract.load(issue)
        if c:
            declared = c.get("kind")
            if not cs.files and not args.base:
                # fall back to the contract's declared paths
                from rusekit.contract import declared_paths
                dp = declared_paths(c)
                if dp:
                    cs = repo.ChangeSet(files=dp, source="change-yaml")

    if cs.source == "none":
        render.warn("not a git repo and no --files given; classifying an empty diff. "
                    "Pass --files or run inside a git checkout for a real result.")

    cl = classify_changeset(cs.files, declared, source=cs.source)
    _report(cl)
    return 0 if cl.ok else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
