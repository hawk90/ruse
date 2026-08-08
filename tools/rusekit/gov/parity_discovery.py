#!/usr/bin/env python3
"""gov parity_discovery — the spec must be answerable to external reality (enforces D-043).

Every other governance checker validates the spec's INTERNAL consistency: parity_coverage proves the
PRD derives from the parity catalog, capability_coverage proves ruse-native surface is not orphaned,
design_backing proves the derivation is not shallow. All three were green while the hand-authored
parity catalog was missing an entire dimension of the Vim input model, because none of them could
see outside spec/. This checker closes that: the machine-derived inventory under
spec/parity/inventory/ is the census of upstream FACT, and it is checkable against its pin.

The load-bearing rule is DISCOVERY STRICT / CLASSIFICATION LAZY:

  discovery       every item in the declared census scope is enumerated, at the pinned revision.
                  Blocking. This is what makes "we didn't know Select mode existed" impossible.
  classification  what ruse intends for an item. `unclassified` is a legitimate resting state and
                  is NOT a finding — forcing it to zero is how a census consumes a year.

...but classification is locked at SURFACE granularity, which is the rule that would have caught the
original failure. The hand-written VIM-MODE table listed all nine modes and still missed that each
mode is a keymap namespace with its own unmatched-key policy. Classifying five items of a surface
while leaving the rest untouched reproduces exactly that: the omission lives in the part you did not
look at. So once a surface is opened, it is opened whole.

FAILS on: inventory/pin drift, an unknown status value, a silently empty surface, a partially
classified surface, or an upstream-removed item that ruse had already committed to without a
`superseded_by`. WARNS on the legacy hand-authored catalog still acting as a PRD source.

One upstream cannot be checked the same way. Neovim publishes its surfaces as tables in-tree, so
its census is a parse and `revision` is a real check. Emacs publishes nothing equivalent — its
surfaces exist only in a running image — so its census is a probe against an installed binary, and
`revision` alone would be decorative: the document could name any sha. Such documents declare
`derived_from: runtime-binary` + `binary_version`, and that version must match the pin's
`version_label`. Same discipline ("a census is answerable to its pin"), applied to the artifact
that actually produced the numbers.
Auto-discovered into `ruse gov check`.
"""
from __future__ import annotations

import argparse
import glob
import os
import re
import sys
from collections import Counter, defaultdict

import yaml

from rusekit import render, repo  # noqa: E402

UPSTREAMS = "spec/parity/upstreams.yaml"
FAMILIES = "spec/parity/families.yaml"
INV_GLOB = "spec/parity/inventory/*/*.yaml"
PRD = "spec/PRD.yaml"
LEGACY_GLOB = "docs/parity/*.md"

STATUSES = {"unclassified", "targeted", "deferred", "unsupported", "intentionally-different"}
# Statuses that mean "ruse committed to this item" — losing one upstream is a migration event.
COMMITTED = {"targeted", "intentionally-different"}
LEGACY_ID_RE = re.compile(r"^(?:VIM|NVIM|EMACS|COM|TERM|WS|REM|ECO|NAT)-[A-Z0-9][A-Z0-9-]*$")


def _norm_version(s: str) -> str:
    """`emacs-30.2`, `v30.2` and `30.2` name the same release; compare the numeric core only."""
    m = re.search(r"\d+(?:\.\d+)*", str(s))
    return m.group(0) if m else str(s)


def _load(path: str) -> dict:
    try:
        with open(repo.path(path), encoding="utf-8") as fh:
            return yaml.safe_load(fh) or {}
    except FileNotFoundError:
        return {}


def inventories() -> list[tuple[str, str, dict]]:
    """(editor, file, parsed) for every generated inventory."""
    out = []
    for path in sorted(glob.glob(repo.path(INV_GLOB))):
        out.append((os.path.basename(os.path.dirname(path)),
                    repo.rel(path),
                    yaml.safe_load(open(path, encoding="utf-8")) or {}))
    return out


def _prd_parity_refs() -> set[str]:
    """Every value under any `parity:` key in the PRD (trace.parity)."""
    acc: set[str] = set()

    def walk(node):
        if isinstance(node, dict):
            for k, v in node.items():
                if k == "parity" and isinstance(v, list):
                    acc.update(str(x) for x in v)
                else:
                    walk(v)
        elif isinstance(node, list):
            for v in node:
                walk(v)

    walk(_load(PRD))
    return acc


def check() -> dict:
    ups = (_load(UPSTREAMS).get("upstreams") or {})
    invs = inventories()

    pin_drift, bad_status, empty_surface, partial, orphan_gone = [], [], [], [], []
    binary_drift = []
    totals = Counter()
    per_surface: dict[str, Counter] = defaultdict(Counter)

    for editor, relpath, doc in invs:
        want = (ups.get(editor) or {}).get("revision")
        got = doc.get("revision")
        if want and got and want != got:
            pin_drift.append((relpath, str(got)[:12], str(want)[:12]))

        # A RUNTIME-DERIVED census has no tree to diff against, so `revision` alone proves nothing:
        # the document could name any sha and be probed from any build. Emacs is censused this way
        # (its surfaces exist only in a running image — upstreams.yaml#source_of_record). So such a
        # document must declare the binary it came from, and that binary must be the pinned release.
        # Without this, the pin discipline that catches Neovim drift is decorative for Emacs.
        if doc.get("derived_from") == "runtime-binary":
            label = (ups.get(editor) or {}).get("version_label") or doc.get("version_label") or ""
            binver = doc.get("binary_version")
            if not binver:
                binary_drift.append((relpath, "(absent)", label))
            elif _norm_version(binver) != _norm_version(label):
                binary_drift.append((relpath, str(binver), label))

        items = doc.get("items") or []
        surface = doc.get("surface") or relpath
        if not items:
            empty_surface.append(relpath)
            continue

        # A surface may be enumerated OR explicitly declared non-enumerable (upstream states the
        # namespace as a rule, not a table — terminal mode). Both are honest; silence is not.
        real = [i for i in items if i.get("enumerable") is not False]
        if not real and not any(i.get("enumerable") is False for i in items):
            empty_surface.append(relpath)

        for i in items:
            st = i.get("status", "unclassified")
            if st not in STATUSES:
                bad_status.append((relpath, i.get("id"), st))
            totals[st] += 1
            key = f"{editor}/{i.get('surface') or surface}"
            per_surface[key][st] += 1
            if i.get("upstream_gone") and st in COMMITTED and not i.get("superseded_by"):
                orphan_gone.append((relpath, i.get("id"), st))

    # Surface-granularity classification lock: opened whole, or not opened.
    for key, counts in sorted(per_surface.items()):
        classified = sum(v for k, v in counts.items() if k != "unclassified")
        unclassified = counts.get("unclassified", 0)
        if classified and unclassified:
            partial.append((key, classified, unclassified))

    # families.yaml declares, per family, what census state each upstream is in. That claim is what a
    # reader trusts when they see `census_status: measured` — and it drifted: six families said
    # `vim: pinned` while coverage.yaml said `not-surveyed`, because nothing compared the two files.
    # A family asserting an upstream is surveyed when no inventory exists is the same defect class as
    # the catalog reporting 100% coverage of a surface it never enumerated, one level up.
    surveyed = {e for e, _, _ in invs}
    family_claims = []
    fam_doc = _load(FAMILIES)
    for fid, fam in sorted((fam_doc.get("families") or {}).items()):
        for editor, claim in sorted((fam.get("upstreams") or {}).items()):
            if editor in ups:
                want = "pinned" if editor in surveyed else "not-surveyed"
            else:
                want = "not-in-census"
            if claim != want:
                family_claims.append((fid, editor, str(claim), want))

    legacy_ids = set()
    for path in sorted(glob.glob(repo.path(LEGACY_GLOB))):
        for line in open(path, encoding="utf-8"):
            line = line.strip()
            if line.startswith("|") and "---" not in line:
                first = line.strip("|").split("|")[0].strip()
                if LEGACY_ID_RE.match(first):
                    legacy_ids.add(first)
    legacy_in_prd = legacy_ids & _prd_parity_refs()

    return {
        "editors": sorted({e for e, _, _ in invs}),
        "files": len(invs),
        "totals": totals,
        "pin_drift": pin_drift,
        "binary_drift": binary_drift,
        "family_claims": family_claims,
        "bad_status": bad_status,
        "empty_surface": empty_surface,
        "partial": partial,
        "orphan_gone": orphan_gone,
        "legacy_in_prd": legacy_in_prd,
        "surfaces": len(per_surface),
    }


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(prog="gov parity_discovery")
    ap.add_argument("--list-unclassified", action="store_true",
                    help="print surfaces with unclassified items (the lazy-classification backlog)")
    args = ap.parse_args(argv or [])

    r = check()
    t = r["totals"]
    discovered = sum(t.values())

    if args.list_unclassified:
        for editor, _, doc in inventories():
            for i in doc.get("items") or []:
                if i.get("status", "unclassified") == "unclassified":
                    print(f"{editor}\t{i.get('surface')}\t{i.get('id')}")
        return 0

    render.heading("gov parity_discovery (upstream census -> spec)")
    if not r["files"]:
        render.warn("no inventory yet — run tools/parity/fetch.py then tools/parity/extract_*.py")
        return 0

    render.field("Surveyed upstreams", ", ".join(r["editors"]) or "none")
    render.field("Items discovered", str(discovered))
    render.field("Classified", f"{discovered - t.get('unclassified', 0)} "
                               f"(unclassified {t.get('unclassified', 0)} — expected, not a finding)")
    render.field("Surfaces", str(r["surfaces"]))

    for relpath, got, want in r["pin_drift"]:
        render.bullet(f"{relpath}: generated from {got} but upstreams.yaml pins {want} — "
                      f"regenerate; a census against an unpinned revision proves nothing", mark="!")
    for relpath, got, want in r["binary_drift"]:
        render.bullet(f"{relpath}: runtime-derived census probed from binary {got} but the pin is "
                      f"{want} — an R-primary surface is only as pinned as the binary it came from; "
                      f"install {want} and regenerate, or bump the pin", mark="!")
    for relpath, iid, st in r["bad_status"]:
        render.bullet(f"{relpath}: {iid} has status '{st}' — must be one of {sorted(STATUSES)}", mark="!")
    for relpath in r["empty_surface"]:
        render.bullet(f"{relpath}: surface enumerated 0 items and is not declared non-enumerable — "
                      f"upstream restructured or the extractor drifted", mark="!")
    for key, c, u in r["partial"]:
        render.bullet(f"{key}: partially classified ({c} classified, {u} unclassified) — a surface is "
                      f"opened whole or not at all; the omission always lives in the part you skipped",
                      mark="!")
    for fid, editor, claim, want in r["family_claims"]:
        render.bullet(f"families.yaml {fid}: claims `{editor}: {claim}` but the evidence layer says "
                      f"`{want}` — a family's upstreams map states the UPSTREAM's census state, and a "
                      f"reader takes census_status: measured on faith from it", mark="!")
    for relpath, iid, st in r["orphan_gone"]:
        render.bullet(f"{relpath}: {iid} is '{st}' but vanished upstream and has no superseded_by", mark="!")

    blocking = (r["pin_drift"] or r["binary_drift"] or r["bad_status"] or r["empty_surface"]
                or r["partial"] or r["orphan_gone"] or r["family_claims"])
    if blocking:
        render.fail("the census is not answerable to its pin")
        return 1

    if r["legacy_in_prd"]:
        render.warn(f"migration debt: {len(r['legacy_in_prd'])} hand-authored parity ID(s) in "
                    f"docs/parity/*.md are still cited by PRD trace.parity. They are unverified "
                    f"against any upstream pin (D-043). Burn down per pillar: classify the census "
                    f"surface, write its contract, repoint trace.parity, drop the legacy row. This "
                    f"becomes blocking per-surface once that surface is classified — not globally, so "
                    f"the first contract does not turn the whole build red")
    render.ok(f"{discovered} upstream items enumerated at their pinned revision; "
              f"{r['surfaces']} surfaces, none partially classified")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
