#!/usr/bin/env python3
"""parity coverage — the honest denominator.

"We surveyed Neovim" is only a checkable claim if three things are recorded together: the exact
revision, the counting METHOD, and the scope the count is over. A bare percentage is decoration —
the same surface yields 557 ex-commands from the static table and 561 from runtime completion, and
neither number is wrong.

Two axes are tracked and must never be collapsed into one:
  DISCOVERY       mechanical. Every item in the declared census scope is enumerated. This is the
                  strict axis and the only exit condition for the census: it is what turns
                  "we didn't know Select mode existed" into an impossible sentence.
  CLASSIFICATION  human. What ruse intends for the item (targeted / deferred / unsupported /
                  intentionally-different). Deliberately LAZY — `unclassified` is a legitimate
                  resting state. Forcing classification to 100% is what makes a census take a year.
  VERIFICATION    executed. An item is `verified` only when a differential fixture passed against
                  the pinned oracle. Nothing is verified by being mentioned in two documents.

  python3 tools/parity/coverage.py            # write spec/parity/coverage.yaml
  python3 tools/parity/coverage.py --show     # print without writing
"""
from __future__ import annotations

import argparse
import glob
import os
import sys
from collections import Counter

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import yaml  # noqa: E402

from rusekit import render, repo  # noqa: E402

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from fetch import load_upstreams  # noqa: E402

INV_GLOB = "spec/parity/inventory/*/*.yaml"
OUT = "spec/parity/coverage.yaml"

STATUSES = ("unclassified", "targeted", "deferred", "unsupported", "intentionally-different")


def load_inventories() -> dict[str, dict[str, dict]]:
    """{editor: {surface_file: parsed}} for every generated inventory on disk."""
    out: dict[str, dict[str, dict]] = {}
    for path in sorted(glob.glob(repo.path(INV_GLOB))):
        editor = os.path.basename(os.path.dirname(path))
        doc = yaml.safe_load(open(path, encoding="utf-8")) or {}
        out.setdefault(editor, {})[os.path.basename(path)] = doc
    return out


def surface_stats(doc: dict) -> dict:
    items = doc.get("items") or []
    by_status = Counter(i.get("status", "unclassified") for i in items)
    non_enum = [i["id"] for i in items if i.get("enumerable") is False]
    gone = [i["id"] for i in items if i.get("upstream_gone")]
    stats = {
        "source_of_record": doc.get("source_of_record"),
        "discovered": len(items),
        "by_status": {k: by_status[k] for k in STATUSES if by_status.get(k)},
        "classified": sum(v for k, v in by_status.items() if k != "unclassified"),
        "unclassified": by_status.get("unclassified", 0),
        "verified": sum(1 for i in items if i.get("verified")),
    }
    if non_enum:
        stats["non_enumerable"] = non_enum
    if gone:
        stats["upstream_gone"] = gone
    return stats


def build(ups_doc: dict, inventories: dict) -> dict:
    ups = ups_doc.get("upstreams") or {}
    out: dict = {
        "version": 1,
        "generated": True,
        "generator": "tools/parity/coverage.py",
        "axes": {
            "discovery": "mechanical; every item in the declared census scope is enumerated. "
                         "The census's only exit condition.",
            "classification": "human and deliberately LAZY; `unclassified` is a legitimate resting "
                              "state. Locked at SURFACE granularity by `ruse gov parity_discovery`.",
            "verification": "executed; `verified` requires a differential fixture that passed "
                            "against the pinned oracle. Being documented in two places is not "
                            "verification.",
        },
        "upstreams": {},
    }
    for editor, spec in sorted(ups.items()):
        inv = inventories.get(editor)
        entry: dict = {
            "revision": spec.get("revision"),
            "version_label": spec.get("version_label"),
            "role": spec.get("role"),
            "denominator_method": (spec.get("denominator") or {}).get("method"),
            "denominator_baseline": (spec.get("denominator") or {}).get("baseline"),
            "census_scope_include": (spec.get("census_scope") or {}).get("include"),
            "census_excluded_surfaces": len((spec.get("census_scope") or {}).get("exclude") or []),
        }
        if not inv:
            entry["status"] = "not-surveyed"
            entry["reason"] = ((spec.get("denominator") or {}).get("note")
                               or "no extractor has been run against this upstream yet")
            out["upstreams"][editor] = entry
            continue
        surfaces = {doc.get("surface") or name: surface_stats(doc) for name, doc in sorted(inv.items())}
        tot_d = sum(s["discovered"] for s in surfaces.values())
        tot_c = sum(s["classified"] for s in surfaces.values())
        tot_v = sum(s["verified"] for s in surfaces.values())
        entry.update({
            "status": "surveyed",
            "surfaces": surfaces,
            "totals": {
                "discovered": tot_d,
                "classified": tot_c,
                "unclassified": tot_d - tot_c,
                "verified": tot_v,
                "classification_pct": round(100.0 * tot_c / tot_d, 1) if tot_d else 0.0,
                "verification_pct": round(100.0 * tot_v / tot_d, 1) if tot_d else 0.0,
            },
            "discovery": {
                "scope_enumerated": "complete",
                "meaning": "every surface in census_scope.include was enumerated, or declared "
                           "non-enumerable with its upstream prose recorded. This is NOT a claim "
                           "about 100% of the editor — it is 100% of the declared scope.",
            },
        })
        out["upstreams"][editor] = entry

    oracles = ups_doc.get("oracles") or {}
    out["oracles"] = {k: {"status": v.get("status"),
                          "hazards": len(v.get("hazards") or [])} for k, v in sorted(oracles.items())}
    out["behavioral"] = {
        "fixtures": 0,
        "verified_items": sum(e.get("totals", {}).get("verified", 0)
                              for e in out["upstreams"].values()),
        "note": "No fixture corpus exists yet. `oracle_selftest` in upstreams.yaml must pass before "
                "any fixture is admitted: three oracle harnesses were tried and three corrupted "
                "their own first observation.",
    }
    return out


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(prog="parity coverage")
    ap.add_argument("--show", action="store_true", help="print the manifest without writing it")
    args = ap.parse_args(argv if argv is not None else sys.argv[1:])

    doc = build(load_upstreams(), load_inventories())

    render.heading("parity coverage (discovery / classification / verification)")
    for editor, e in doc["upstreams"].items():
        if e.get("status") != "surveyed":
            render.bullet(f"{editor:<8} {e.get('status')} — {str(e.get('reason'))[:78]}", mark="!")
            continue
        t = e["totals"]
        render.bullet(f"{editor:<8} @{e['version_label']}  discovered {t['discovered']}  "
                      f"classified {t['classified']} ({t['classification_pct']}%)  "
                      f"verified {t['verified']} ({t['verification_pct']}%)")
        for sname, s in e["surfaces"].items():
            extra = "  [non-enumerable surface declared]" if s.get("non_enumerable") else ""
            render.bullet(f"  {sname:<12} {s['discovered']:>5} discovered · "
                          f"{s['unclassified']:>5} unclassified{extra}")

    if args.show:
        print()
        print(yaml.safe_dump(doc, sort_keys=False, allow_unicode=True, width=110))
        return 0

    header = (
        "# GENERATED — regenerate with `python3 tools/parity/coverage.py`.\n"
        "# The number that matters is DISCOVERY, not implementation. `unsupported` and `deferred`\n"
        "# are honest outcomes; an item nobody knew existed is not. classification_pct is expected\n"
        "# to sit near zero for a long time and that is the design (classification is lazy).\n"
    )
    with open(repo.path(OUT), "w", encoding="utf-8") as fh:
        fh.write(header)
        yaml.safe_dump(doc, fh, sort_keys=False, allow_unicode=True, width=110)
    render.ok(f"wrote {OUT}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
