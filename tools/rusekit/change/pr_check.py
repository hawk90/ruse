"""pr check — the merge-gate policy for a change.

Composes the pieces into one pass/fail: classify must hold, the effective kind's required
artifacts must be present, the diff must stay inside allow_paths, and (for risk ≥ 2)
recorded green evidence must exist. This is what a CI `change-policy` job runs.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import sys

from rusekit import repo, render, contract, model as model_mod  # noqa: E402
from rusekit.change.classify import classify_changeset  # noqa: E402


def _match_any(path: str, prefixes: list[str]) -> bool:
    return any(path == p or path.startswith(p.rstrip("*")) for p in prefixes)


def _evidence_steps(issue: str) -> dict[str, int]:
    """Map recorded command→exit_code from evidence.json (by verify step name)."""
    p = os.path.join(repo.work_dir(issue), "evidence.json")
    if not os.path.isfile(p):
        return {}
    try:
        data = json.load(open(p, encoding="utf-8"))
    except Exception:
        return {}
    steps = {}
    for cmd in data.get("commands", []):
        name = cmd.get("step") or cmd.get("command", "")
        steps[name] = cmd.get("exit_code", 1)
    return steps


def _contract_from_body(text: str) -> dict | None:
    """Parse the author-declared gate block (```json after `ruse-gate:v1`) from a PR body. This is
    UNTRUSTED input — the caller re-derives observed kind + blast radius from the real diff."""
    m = re.search(r"ruse-gate:v1.*?```json\s*(\{.*?\})\s*```", text, re.S)
    if not m:
        return None
    gate = json.loads(m.group(1))
    gate.pop("v", None)   # block-format version, not a contract field
    return gate


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(prog="ruse pr check")
    ap.add_argument("--issue", help="change workspace to check (default: active)")
    ap.add_argument("--base", help="git ref to diff against")
    ap.add_argument("--files", nargs="*", help="explicit file list")
    ap.add_argument("--pr-body", help="CI mode: read the declared contract from a PR body file "
                                      "(gate of record = diff + re-run verify; local .ruse is untrusted)")
    args = ap.parse_args(argv)

    ci = bool(args.pr_body)
    if ci:
        try:
            body = open(args.pr_body, encoding="utf-8").read()
        except OSError as e:
            render.fail(f"cannot read --pr-body {args.pr_body}: {e}")
            return 1
        c = _contract_from_body(body)
        if c is None:
            render.fail("no `ruse-gate:v1` machine block in the PR body — regenerate it with `ruse pr render`")
            return 1
        issue = (c.get("artifacts") or {}).get("issue") or args.issue or "PR"
    else:
        issue = args.issue or repo.active_issue()
        if not issue:
            render.fail("no change workspace found; run `ruse change start` first")
            return 1
        c = contract.load(issue)
        if c is None:
            render.fail(f"no change.yaml for issue {issue}")
            return 1

    m = model_mod.load()
    checks: list[tuple[bool, str]] = []          # (passed, message)
    warns: list[str] = []

    # contract structural validity
    errs, cwarns = contract.validate(c, m)
    for e in errs:
        checks.append((False, f"contract: {e}"))
    warns += cwarns

    # changeset
    cs = repo.resolve_changeset(base=args.base, files=args.files)
    if cs.source == "none":
        cs = repo.ChangeSet(files=contract.declared_paths(c), source="change-yaml")
        warns.append("no git/--files: checking against change.yaml declared paths only")

    declared = c.get("kind")
    cl = classify_changeset(cs.files, declared, source=cs.source)
    checks.append((cl.ok, f"classify: declared '{declared}' vs observed "
                          f"'{cl.observed_kind or 'none'}'"
                          + ("" if cl.ok else " — declared too low or generated file edited")))

    # effective kind = higher risk of declared vs observed
    policy = contract.load_kinds()
    kinds = policy.get("kinds") or {}
    eff_kind = declared
    if cl.observed_kind and (cl.observed_risk > (contract.kind_risk(declared) or -1)):
        eff_kind = cl.observed_kind
    kspec = kinds.get(eff_kind, {})
    eff_risk = kspec.get("risk", 0)

    # required artifacts
    art = c.get("artifacts") or {}
    aff = c.get("affected") or {}
    for req in kspec.get("required_artifacts", []):
        if req == "issue":
            checks.append((bool(art.get("issue")), "artifact: issue linked"))
        elif req == "rfc":
            rfc = art.get("rfc")
            checks.append((bool(rfc) and m.has(rfc),
                           f"artifact: RFC present & resolves ({rfc or 'missing'})"))
        elif req == "impact":
            if ci:
                checks.append((bool(aff), "artifact: blast radius declared in gate block (affected IDs)"))
            else:
                ip = os.path.join(repo.work_dir(issue), "impact.json")
                has = os.path.isfile(ip) and bool((json.load(open(ip)) or {}).get("roots"))
                checks.append((has, "artifact: impact.json recorded (run `ruse impact --out`)"))
        elif req == "spec_ref":
            linked = bool(contract.affected_ids(c) or art.get("rfc") or art.get("decision"))
            checks.append((linked, "artifact: a spec/Decision ID is linked"))
        elif req == "compat":
            # P0: no protocol_compat tool yet — require an explicit human attestation field.
            if art.get("compat_reviewed"):
                checks.append((True, "artifact: compatibility reviewed (attested)"))
            else:
                warns.append("artifact: compatibility review required — set "
                             "artifacts.compat_reviewed: true after review (tool is P1)")
        elif req == "migration":
            checks.append((art.get("migration") is not None, "artifact: migration decided"))

    # allow_paths blast radius
    allow = c.get("allow_paths") or []
    forbid = c.get("forbid_paths") or []
    if allow:
        stray = [f for f in cs.files if not _match_any(f, allow)]
        checks.append((not stray, "blast radius: all files within allow_paths"
                       + (f" (stray: {stray})" if stray else "")))
    hits = [f for f in cs.files if _match_any(f, forbid)] if forbid else []
    if hits:
        checks.append((False, f"blast radius: touched forbid_paths {hits}"))

    # evidence (risk >= 2). Recorded evidence must exist and NONE may be failing; a
    # required step that was never run is a warning (e.g. heavy cargo steps deferred),
    # not a hard block — the human decides whether the missing evidence is acceptable.
    if eff_risk >= 2 and ci:
        # Local evidence.json is fabricatable, so it is NEVER the server-side gate of record. In CI the
        # required verify steps are separate REQUIRED JOBS (branch protection) that re-run from scratch.
        checks.append((True, "evidence: gate of record is CI's own verify jobs "
                             f"({', '.join(kspec.get('evidence', []) or ['verify'])}), not local evidence.json"))
    elif eff_risk >= 2:
        steps = _evidence_steps(issue)
        required_ev = kspec.get("evidence", [])
        if not steps:
            checks.append((False, "evidence: none recorded (run `ruse verify` to record)"))
        else:
            missing = [s for s in required_ev if s not in steps]
            failed = [s for s, code in steps.items() if code != 0]
            checks.append((not failed,
                           f"evidence: {len(steps)} recorded, none failing"
                           + (f", failing {failed}" if failed else "")))
            if missing:
                warns.append(f"evidence: required steps not yet run: {missing} "
                             "(run `ruse verify --full` before merge)")

    # ---- report ----
    render.heading("pr check")
    render.field("Issue", str(issue))
    render.field("Declared kind", declared or "—")
    render.field("Effective kind", f"{eff_kind} (risk {eff_risk})")
    render.field("Changed files", f"{len(cs.files)} (source: {cs.source})")
    render.field("Mode", "CI — diff + PR body (local .ruse untrusted)" if ci else "local preflight")
    print()
    passed = True
    for ok_, msg in checks:
        (render.ok if ok_ else render.fail)(msg)
        passed = passed and ok_
    for w in warns:
        render.warn(w)
    print()
    if passed:
        render.ok("pr check: all required gates satisfied")
        return 0
    render.fail("pr check: one or more gates failed")
    return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
