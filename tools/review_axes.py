#!/usr/bin/env python3
"""
review-axes — checker & query tool for spec/review-axes.yaml (ruse's review-axis rubric).

The catalog holds axis *definitions* (the questions). Assessment *results* (the answers) live in a dated
report under docs/reviews/ — never in the catalog (one fact, one home).

Usage:
  python3 tools/review_axes.py                 # validate the catalog (exit 1 on error)
  python3 tools/review_axes.py --stats         # counts by domain / tier / method + machine-automatable share
  python3 tools/review_axes.py --list          # list axes (filter with --tier/--domain/--method)
  python3 tools/review_axes.py --list --tier P0 --domain ARCH --method machine
  python3 tools/review_axes.py --json          # emit the fully-resolved catalog as JSON (generated view; not committed)

Importable: spec-validate calls `load_catalog()` + `validate_catalog()` so there is a single verification
entry point (RA-CICD-002). Pure functions live at module scope; only the CLI is under __main__.
"""
import sys, os, re, json

try:
    import yaml
except ImportError:
    print("FAIL: PyYAML not installed (pip install pyyaml)"); sys.exit(1)

CATALOG = "spec/review-axes.yaml"
METHODS = {"machine", "llm", "manual", "mixed"}
TIERS = {"P0", "P1", "P2", "P3"}
ID_RE = re.compile(r"^RA-([A-Z]+)-(\d{3})$")


def load_catalog(path=CATALOG):
    with open(path) as f:
        return yaml.safe_load(f)


def iter_axes(cat):
    """Yield each axis as a dict with domain + inherited defaults resolved."""
    for dom in cat.get("domains", []) or []:
        dcode = dom.get("id", "")
        for ax in dom.get("axes", []) or []:
            a = dict(ax)
            a["domain"] = dcode
            a["domain_title"] = dom.get("title", "")
            a.setdefault("tier", dom.get("default_tier"))
            a.setdefault("method", dom.get("default_method"))
            yield a


def validate_catalog(cat):
    """Return (errors, warnings). Structural + referential integrity of the catalog itself."""
    errors, warnings = [], []
    if cat is None:
        return (["catalog is empty / did not parse"], warnings)
    if cat.get("version") != 1:
        warnings.append(f"catalog version is {cat.get('version')!r}, expected 1")

    all_ids = set()
    for a in iter_axes(cat):
        all_ids.add(a.get("id"))

    seen = set()
    domains = cat.get("domains", []) or []
    if not domains:
        errors.append("no domains defined")
    for dom in domains:
        dcode = dom.get("id", "")
        if not re.fullmatch(r"[A-Z]+", dcode or ""):
            errors.append(f"domain id {dcode!r} must be ALL-CAPS letters")
        if not (dom.get("title") or "").strip():
            errors.append(f"domain {dcode}: empty title")
        dm, dt = dom.get("default_method"), dom.get("default_tier")
        if dm is not None and dm not in METHODS:
            errors.append(f"domain {dcode}: bad default_method {dm!r}")
        if dt is not None and dt not in TIERS:
            errors.append(f"domain {dcode}: bad default_tier {dt!r}")

        for ax in dom.get("axes", []) or []:
            aid = ax.get("id", "")
            m = ID_RE.fullmatch(aid or "")
            if not m:
                errors.append(f"axis id {aid!r} must match RA-<DOMAIN>-NNN")
            elif m.group(1) != dcode:
                errors.append(f"axis {aid}: domain segment != enclosing domain {dcode}")
            if aid in seen:
                errors.append(f"duplicate axis id {aid}")
            seen.add(aid)
            if not (ax.get("title") or "").strip():
                errors.append(f"axis {aid}: empty title")
            method = ax.get("method", dm)
            tier = ax.get("tier", dt)
            if method not in METHODS:
                errors.append(f"axis {aid}: unresolved/bad method {method!r}")
            if tier not in TIERS:
                errors.append(f"axis {aid}: unresolved/bad tier {tier!r}")
            for r in ax.get("refs", []) or []:
                if r not in all_ids:
                    errors.append(f"axis {aid}: ref {r!r} resolves to no axis id")
    return errors, warnings


def stats(cat):
    axes = list(iter_axes(cat))
    by = lambda key: {k: sum(1 for a in axes if a.get(key) == k) for k in sorted({a.get(key) for a in axes})}
    dom = {}
    for a in axes:
        dom[a["domain"]] = dom.get(a["domain"], 0) + 1
    return {
        "total": len(axes),
        "by_tier": by("tier"),
        "by_method": by("method"),
        "by_domain": dom,
        "machine_share": round(sum(1 for a in axes if a["method"] == "machine") / max(1, len(axes)), 3),
    }


def _filtered(cat, tier=None, domain=None, method=None):
    for a in iter_axes(cat):
        if tier and a.get("tier") != tier:
            continue
        if domain and a.get("domain") != domain:
            continue
        if method and a.get("method") != method:
            continue
        yield a


def main(argv):
    args = set(argv)
    def opt(name):
        if name in argv:
            i = argv.index(name)
            return argv[i + 1] if i + 1 < len(argv) else None
        return None

    try:
        cat = load_catalog()
    except FileNotFoundError:
        print(f"FAIL: {CATALOG} not found (run from repo root)"); return 1

    if "--json" in args:
        print(json.dumps({"version": cat.get("version"), "axes": list(iter_axes(cat))}, indent=2, ensure_ascii=False))
        return 0

    if "--stats" in args:
        s = stats(cat)
        print(f"review-axes: {s['total']} axes across {len(s['by_domain'])} domains")
        print(f"  by tier   : {s['by_tier']}")
        print(f"  by method : {s['by_method']}  (machine-automatable share = {s['machine_share']})")
        print(f"  by domain : {s['by_domain']}")
        errors, warnings = validate_catalog(cat)
        for w in warnings: print("WARN:", w)
        if errors:
            print(f"\nFAIL ({len(errors)} errors):")
            for e in errors: print("  ", e)
            return 1
        return 0

    if "--list" in args:
        rows = list(_filtered(cat, opt("--tier"), opt("--domain"), opt("--method")))
        for a in rows:
            print(f"{a['id']:<16} [{a['tier']} {a['method']:<7}] {a['title']}")
        print(f"\n{len(rows)} axes")
        return 0

    # default: validate
    errors, warnings = validate_catalog(cat)
    n = sum(1 for _ in iter_axes(cat))
    for w in warnings: print("WARN:", w)
    if errors:
        print(f"review-axes: FAIL ({len(errors)} errors)")
        for e in errors: print("  ", e)
        return 1
    print(f"review-axes: PASS ({n} axes)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
