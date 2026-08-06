#!/usr/bin/env python3
"""
phases — checker & query tool for spec/phases.yaml (the delivery-phase ladder).

PORTABLE: the engine is repo-agnostic; it reads a repo's spec/phases.yaml and the feature buckets in
spec/PRD.yaml. spec/phases.yaml owns the *fine* ordering; PRD F-* `stage` owns the *coarse* bucket. This
tool proves the former is a clean refinement of the latter (one fact, one home — the two are cross-checked,
never hand-synced).

Usage:
  python3 tools/phases.py                 # validate (partition + stage-refinement + order); exit 1 on error
  python3 tools/phases.py --list          # list phases and their features
  python3 tools/phases.py --order         # per phase, features with their component deps (impl-order rationale)

Importable: spec-validate can call load() + validate() so there is a single verification entry point.
Pure functions live at module scope; only the CLI is under __main__.
"""
import sys, os, re, argparse

try:
    import yaml
except ImportError:
    print("FAIL: PyYAML not installed (pip install pyyaml)"); sys.exit(1)

PHASES = "spec/phases.yaml"
PRD = "spec/PRD.yaml"
STAGES = ["mvp", "post-mvp", "future"]   # coarse delivery buckets, in order
FID_RE = re.compile(r"^F-\d+$")


def load(path=PHASES):
    with open(path) as f:
        return yaml.safe_load(f)


CID_RE = re.compile(r"^C-[A-Z]+$")


def _prd_map(pat, path):
    doc = yaml.safe_load(open(path)) or {}
    for v in doc.values():
        if isinstance(v, dict) and any(pat.match(str(k)) for k in v):
            return {k: (val or {}) for k, val in v.items() if pat.match(str(k))}
    return {}


def load_features(path=PRD):
    """Return {F-id: {stage, depends_on, ...}} from PRD."""
    return _prd_map(FID_RE, path)


def load_components(path=PRD):
    """Return {C-id: {depends_on, ...}} from PRD."""
    return _prd_map(CID_RE, path)


def validate(cat, features, components=None):
    """Return (errors, warnings): structure, referential integrity, partition, stage-refinement, order,
    and (when components are given) cross-phase component forward-dependencies."""
    errors, warnings = [], []
    if cat is None:
        return (["spec/phases.yaml is empty / did not parse"], warnings)
    if cat.get("version") != 1:
        warnings.append(f"version is {cat.get('version')!r}, expected 1")

    phases = cat.get("phases") or []
    if not phases:
        return (["no phases defined"], warnings)

    all_fids = set(features)
    seen, prev_stage_idx = {}, -1
    for i, ph in enumerate(phases):
        pid = ph.get("id") or f"<phase #{i}>"
        if not re.match(r"^[a-z][a-z0-9-]*$", str(ph.get("id") or "")):
            errors.append(f"{pid}: id must be a lower-kebab slug (not P0/P1 — that is rollout priority)")
        stage = ph.get("stage")
        if stage not in STAGES:
            errors.append(f"{pid}: stage {stage!r} not in {STAGES}")
        else:
            idx = STAGES.index(stage)
            if idx < prev_stage_idx:
                errors.append(f"{pid}: stage '{stage}' appears after a later stage — phases must be ordered "
                              f"{' < '.join(STAGES)}")
            prev_stage_idx = max(prev_stage_idx, idx)
        for fid in ph.get("includes") or []:
            if fid not in all_fids:
                errors.append(f"{pid}: includes unknown feature {fid}")
                continue
            if fid in seen:
                errors.append(f"{fid} is in two phases ({seen[fid]} and {pid}) — a feature ships once")
            seen[fid] = pid
            fstage = features[fid].get("stage")
            if stage in STAGES and fstage != stage:
                errors.append(f"{fid}: PRD stage '{fstage}' != phase '{pid}' stage '{stage}' "
                              f"— phase grouping must refine stage, not cross it")

    missing = sorted(all_fids - set(seen))
    if missing:
        errors.append(f"{len(missing)} feature(s) in no phase: {', '.join(missing)}")

    # Cross-phase forward-dependency (components). A component's phase = the earliest phase whose features
    # first require it; a component may not depend on one that only arrives in a later phase (that would make
    # the earlier phase unbuildable). Legitimate cross-phase deps are allowlisted in `cross_phase_deps_allowed`.
    if components is not None:
        names = [p.get("id") for p in phases]
        order = {p.get("id"): i for i, p in enumerate(phases)}
        allow = {(a.get("from"), a.get("to")) for a in (cat.get("cross_phase_deps_allowed") or [])}
        comp_phase = {}
        for fid, pid in seen.items():
            for c in (features.get(fid, {}).get("depends_on") or []):
                comp_phase[c] = min(comp_phase.get(c, 10 ** 9), order.get(pid, 10 ** 9))
        for cid, meta in sorted(components.items()):
            if cid not in comp_phase:
                warnings.append(f"component {cid} is required by no phased feature (not placed in a phase)")
                continue
            for dep in (meta.get("depends_on") or []):
                if dep in comp_phase and comp_phase[dep] > comp_phase[cid] and (cid, dep) not in allow:
                    errors.append(
                        f"forward-dep: {cid} ({names[comp_phase[cid]]}) depends on {dep} "
                        f"({names[comp_phase[dep]]}) — a later phase. Fix the dep, re-phase, or add "
                        f"{{from: {cid}, to: {dep}}} to phases.yaml `cross_phase_deps_allowed` with a reason.")
    return (errors, warnings)


def _cmd_default():
    cat, feats, comps = load(), load_features(), load_components()
    errors, warnings = validate(cat, feats, comps)
    for w in warnings:
        print(f"WARN {w}")
    if errors:
        for e in errors:
            print(f"FAIL {e}")
        print(f"\nphases: FAIL ({len(errors)} error(s))")
        return 1
    n = sum(len(p.get("includes") or []) for p in cat["phases"])
    print(f"phases: PASS ({len(cat['phases'])} phases, {n} features, refines stage cleanly)")
    return 0


def _cmd_list():
    cat = load()
    for ph in cat.get("phases") or []:
        inc = ", ".join(ph.get("includes") or [])
        print(f"[{ph.get('stage'):8}] {ph.get('id'):16} {ph.get('title','')}")
        print(f"           {inc}")
    return 0


def _cmd_order():
    cat, feats = load(), load_features()
    for ph in cat.get("phases") or []:
        print(f"\n== {ph.get('id')} ({ph.get('stage')}) ==")
        # impl-order rationale: fewer component deps first (a lightweight proxy; full topo needs the
        # component graph, which lives in PRD C-* depends_on — wire that in when components are phased).
        rows = [(fid, feats.get(fid, {}).get("depends_on") or []) for fid in ph.get("includes") or []]
        for fid, deps in sorted(rows, key=lambda r: (len(r[1]), r[0])):
            print(f"  {fid}  <- {', '.join(deps) if deps else '(no component deps)'}")
    return 0


def main(argv):
    ap = argparse.ArgumentParser(prog="phases", description="delivery-phase ladder checker")
    ap.add_argument("--list", action="store_true", help="list phases and their features")
    ap.add_argument("--order", action="store_true", help="per-phase features with component deps")
    args = ap.parse_args(argv)
    if args.list:
        return _cmd_list()
    if args.order:
        return _cmd_order()
    return _cmd_default()


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
