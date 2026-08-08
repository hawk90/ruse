#!/usr/bin/env python3
"""gov families_cover — the family taxonomy must be self-consistent, and any classified item must land in a
family its surface actually permits (enforces spec/parity/families.yaml).

families.yaml is the classification vocabulary between the census and the PRD, but nothing enforced it: a
PILLAR could carry no evidence_refs, a surface_cover could name a family that does not exist, or an inventory
item could be tagged with a family its surface never declared — all silently. Every earlier gov gate checks a
CONSUMER (config->PRD, parity->PRD); this one checks the DECLARATION itself plus the tags that key off it,
because a classification layer whose own contract is unchecked is the D-043 mistake wearing a new hat.

Two halves:

  1. DECLARATION INTEGRITY. The id prefix encodes the completion axis and each axis carries an obligation, so
     the file's own rules are made executable:
       FAM-*    parity-fact  -> must declare `covers_surfaces`
       PILLAR-* ruse-choice  -> must declare `evidence_refs` (census) or a `contract` (architecture pillar)
       POL-*    policy       -> must declare a `lint_id`
     A prefix no axis claims, or a declared `axis` disagreeing with the prefix, FAILS. Every family named in
     `surface_cover` must be a real declared family; a `forbid_family_ids` id must not also be a family.

  2. TAG LEGALITY (classification is LAZY, but a tag that exists must be legal). An inventory item MAY carry
     `family:` (absence is the legitimate resting state and is never a finding). If it does: the family must
     exist, must not be forbidden, and must appear in its surface's surface_cover primary/secondary set — an
     item cannot claim a family its surface never partitioned. Same for optional `secondary:` tags.

Distribution is advisory: on a surface whose cover is a MULTI-family partition, one family owning more than
`max_primary_share` of the tagged items is a WARN to split — suppressed where the cover is single-family by
design (100% is then correct). `cover_validated` counts the tags that passed.

FAILS on any declaration breach or illegal tag. WARNs on distribution skew or a tagged surface with no cover
entry. Never fails on absent tags. Auto-discovered into `ruse gov check`.
"""
from __future__ import annotations

import argparse
import glob
import os
import sys
from collections import Counter, defaultdict

import yaml

from rusekit import render, repo  # noqa: E402

FAMILIES = "spec/parity/families.yaml"
INV_GLOB = "spec/parity/inventory/*/*.yaml"

# id prefix -> the axis it commits to, and the field that axis obliges it to declare.
AXIS_BY_PREFIX = {"FAM": "parity-fact", "PILLAR": "ruse-choice", "POL": "policy"}
OBLIGATION = {"FAM": "covers_surfaces", "PILLAR": "evidence_refs", "POL": "lint_id"}


def _load(path: str) -> dict:
    with open(repo.path(path), encoding="utf-8") as fh:
        return yaml.safe_load(fh) or {}


def load_families() -> dict:
    doc = _load(FAMILIES)
    cons = ((doc.get("cover_validation") or {}).get("constraints")) or {}
    return {
        "families": doc.get("families") or {},
        "surface_cover": doc.get("surface_cover") or {},
        "forbid": set(cons.get("forbid_family_ids") or []),
        "max_share": cons.get("max_primary_share", 0.4),
    }


def check_declaration(F: dict) -> dict:
    ids = set(F["families"])
    bad_axis, missing_obl, cover_dangling, forbid_clash = [], [], [], []

    for fid, body in F["families"].items():
        body = body or {}
        prefix = fid.split("-", 1)[0]
        want_axis = AXIS_BY_PREFIX.get(prefix)
        if want_axis is None:
            bad_axis.append((fid, "id prefix names no axis (want FAM-/PILLAR-/POL-)"))
            continue
        if body.get("axis") != want_axis:
            bad_axis.append((fid, f"axis '{body.get('axis')}' contradicts '{want_axis}' implied by prefix"))
        # A ruse-choice PILLAR is evidenced by census refs OR — for an architecture pillar with no
        # enumerable surface — by a contract. FAM/POL have a single obliged field.
        if prefix == "PILLAR":
            if not (body.get("evidence_refs") or body.get("contract")):
                missing_obl.append((fid, "ruse-choice must declare `evidence_refs` (census) or a `contract`"))
        elif not body.get(OBLIGATION[prefix]):
            missing_obl.append((fid, f"{want_axis} must declare a non-empty `{OBLIGATION[prefix]}`"))

    for fid in F["forbid"]:
        if fid in ids:
            forbid_clash.append(fid)

    for surface, entry in F["surface_cover"].items():
        entry = entry or {}
        for role in ("primary", "secondary"):
            for fid in (entry.get(role) or []):
                if fid not in ids:
                    cover_dangling.append((surface, role, fid))

    return {"bad_axis": bad_axis, "missing_obl": missing_obl,
            "cover_dangling": cover_dangling, "forbid_clash": forbid_clash}


def _allowed(cover: dict, surface: str):
    """The families surface_cover permits for a surface; sub-surfaces (mode_key.insert) map to their root."""
    entry = cover.get(surface) or cover.get(surface.split(".")[0]) if surface else None
    if entry is None:
        return None
    return set(entry.get("primary") or []) | set(entry.get("secondary") or [])


def _inventories():
    for path in sorted(glob.glob(repo.path(INV_GLOB))):
        yield repo.rel(path), yaml.safe_load(open(path, encoding="utf-8")) or {}


def check_tags(F: dict) -> dict:
    ids = set(F["families"])
    illegal, forbidden, no_cover = [], [], []
    validated = 0
    dist: dict[str, Counter] = defaultdict(Counter)  # root surface -> primary family -> count

    for rel, doc in _inventories():
        for item in (doc.get("items") or []):
            surface = item.get("surface") or ""
            tags = [("family", item["family"])] if item.get("family") else []
            tags += [("secondary", s) for s in (item.get("secondary") or [])]
            if not tags:
                continue
            allowed = _allowed(F["surface_cover"], surface)
            for role, fid in tags:
                if fid in F["forbid"]:
                    forbidden.append((rel, item.get("id"), fid))
                elif fid not in ids:
                    illegal.append((rel, item.get("id"), fid, "not a declared family"))
                elif allowed is None:
                    no_cover.append((rel, surface, fid))
                elif fid not in allowed:
                    illegal.append((rel, item.get("id"), fid,
                                    f"not in surface_cover[{surface.split('.')[0]}]"))
                else:
                    validated += 1
                    if role == "family":
                        dist[surface.split(".")[0]][fid] += 1

    return {"illegal": illegal, "forbidden": forbidden, "no_cover": no_cover,
            "validated": validated, "dist": dist}


def skew_warnings(F: dict, dist: dict) -> list:
    out = []
    for surface, counts in sorted(dist.items()):
        primary_decl = (F["surface_cover"].get(surface) or {}).get("primary") or []
        if len(primary_decl) <= 1:
            continue  # single-family by design; 100% is expected, not a smell
        total = sum(counts.values())
        fam, n = counts.most_common(1)[0]
        if total and n / total > F["max_share"]:
            out.append((surface, fam, n, total, n / total))
    return out


def main(argv=None) -> int:
    argparse.ArgumentParser(prog="gov families_cover").parse_args(argv or [])
    F = load_families()
    d = check_declaration(F)
    t = check_tags(F)

    render.heading("gov families_cover (family taxonomy + classification tags)")
    render.field("Families declared", str(len(F["families"])))
    render.field("Tags validated", str(t["validated"]))
    render.field("Surfaces opened", str(len(t["dist"])))

    for fid, why in d["bad_axis"]:
        render.bullet(f"{fid}: {why}", mark="!")
    for fid, why in d["missing_obl"]:
        render.bullet(f"{fid}: {why}", mark="!")
    for surface, role, fid in d["cover_dangling"]:
        render.bullet(f"surface_cover[{surface}].{role}: '{fid}' is not a declared family", mark="!")
    for fid in d["forbid_clash"]:
        render.bullet(f"'{fid}' is in forbid_family_ids yet also defined as a family", mark="!")
    for rel, iid, fid in t["forbidden"]:
        render.bullet(f"{rel}: {iid} tagged forbidden family '{fid}'", mark="!")
    for rel, iid, fid, why in t["illegal"]:
        render.bullet(f"{rel}: {iid} family '{fid}' {why}", mark="!")

    if (d["bad_axis"] or d["missing_obl"] or d["cover_dangling"] or d["forbid_clash"]
            or t["forbidden"] or t["illegal"]):
        render.fail("family taxonomy or a classification tag is not answerable to families.yaml")
        return 1

    for rel, surface, fid in t["no_cover"]:
        render.warn(f"{rel}: surface '{surface}' carries a family tag ('{fid}') but has no surface_cover entry")
    for surface, fam, n, total, share in skew_warnings(F, t["dist"]):
        render.warn(f"{surface}: {fam} owns {n}/{total} ({share:.0%}) of tags — over max_primary_share; "
                    f"a multi-family cover this skewed is a signal to split")
    render.ok(f"family taxonomy self-consistent; {t['validated']} classification tag(s) legal "
              f"({len(F['families'])} families, {len(t['dist'])} surface(s) opened)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
