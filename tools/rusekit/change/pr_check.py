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
from rusekit.change.edit import CONTRACT_FLAGS  # noqa: E402


# Automation whose PRs never carry a hand-authored gate block. Rather than exempt them blindly,
# `pr check` synthesizes a self-consistent contract from the diff (declared = observed kind) and
# still enforces the artifacts that kind requires — so a bot straying into a high-risk path fails.
TRUSTED_BOTS = {"dependabot[bot]", "renovate[bot]"}
# A trusted bot whose whole diff is these dependency manifests is doing a version bump, so it needs no issue.
# Humans keep the path_triggers floor: a member Cargo.toml edit by a person stays `implementation`.
BOT_MANIFESTS = re.compile(
    r"^((apps|crates)/[^/]+/)?Cargo\.toml$|^(Cargo\.lock|pyproject\.toml|rust-toolchain\.toml)$"
)


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
                                      "(gate of record = diff + re-run verify; local work dir is untrusted)")
    ap.add_argument("--actor", help="CI: PR author login; a TRUSTED_BOTS actor may pass without a gate block")
    args = ap.parse_args(argv)

    ci = bool(args.pr_body)
    bot_note: str | None = None
    manifest_bump = False
    if ci:
        try:
            with open(args.pr_body, encoding="utf-8") as fh:
                body = fh.read()
        except OSError as e:
            render.fail(f"cannot read --pr-body {args.pr_body}: {e}")
            return 1
        c = _contract_from_body(body)
        if c is None:
            actor = (args.actor or "").strip()
            if actor in TRUSTED_BOTS:
                bcs = repo.resolve_changeset(base=args.base, files=args.files)
                bcl = classify_changeset(bcs.files, None, source=bcs.source)
                manifest_bump = bool(bcs.files) and all(BOT_MANIFESTS.match(f) for f in bcs.files)
                derived = "build" if manifest_bump else (bcl.observed_kind or "build")
                c = {"kind": derived, "goal": f"automated change by {actor}", "affected": {},
                     "artifacts": {}, "allow_paths": [], "forbid_paths": [], "contracts": {}}
                bot_note = (f"trusted bot '{actor}': no gate block — auto-declared kind '{derived}' from "
                            "the diff; required artifacts for that kind are still enforced")
            else:
                render.fail("no `ruse-gate:v1` machine block in the PR body — "
                            "regenerate it with `ruse pr render`")
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
    if bot_note:
        warns.append(bot_note)

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
    # A bot's manifest bump is `build` by the exemption above, so path floors must not raise it back.
    cl = classify_changeset([] if manifest_bump else cs.files, declared, source=cs.source)
    checks.append((cl.ok, f"classify: declared '{declared}' vs observed "
                          f"'{cl.observed_kind or 'none'}'"
                          + ("" if cl.ok else " — declared too low or generated file edited")))

    # effective kind = higher risk of declared vs observed
    policy = contract.load_kinds()
    kinds = policy.get("kinds") or {}
    eff_kind = declared
    if cl.observed_kind and (cl.observed_risk > (contract.kind_risk(declared) or -1)):
        eff_kind = cl.observed_kind
    # contracts.* flags (#1204): a change that declares it crosses a stability boundary is a `contract`
    # change (spec/templates/change.yaml), whatever the paths say; a stored-format change also names its
    # migration. A path that suggests a flag the contract leaves false is a warning: the diff cannot tell
    # a format change from a refactor of the code that reads the format.
    con = c.get("contracts") or {}
    art = c.get("artifacts") or {}
    crossed = [k for k in CONTRACT_FLAGS if con.get(k) is True]
    floor = contract.kind_risk("contract")
    if crossed and floor is not None:
        declared_risk = contract.kind_risk(declared) if declared else None
        ok_kind = declared_risk is not None and declared_risk >= floor
        checks.append((ok_kind, f"contracts: {', '.join(crossed)} set, so the kind is 'contract'"
                       + ("" if ok_kind else f" — declared '{declared}' is too low "
                          "(`change set --kind contract`)")))
        if (contract.kind_risk(eff_kind) or 0) < floor:
            eff_kind = "contract"
    if con.get("persistent_format") is True:
        mig = art.get("migration")
        named = isinstance(mig, str) and bool(mig.strip())
        checks.append((named, "contracts: persistent-format change names its migration"
                       + (f" ({mig})" if named else
                          " — `change set --migration <ref>` (or `--migration none`)")))
    for f, flag, why in cl.flag_hits:
        if con.get(flag) is not True:
            warns.append(f"contracts: {f} ({why}) — if this changes the format, "
                         f"`change set --{flag.replace('_', '-')}`")
    kspec = kinds.get(eff_kind, {})
    eff_risk = kspec.get("risk", 0)

    # required artifacts
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
            # No protocol-compat tool yet (verify NOT_YET), so the artifact is a human attestation: where the
            # compatibility review is recorded. It was a warning, so `contract` could never fail on it (#887).
            cr = art.get("compat_reviewed")
            named = (isinstance(cr, str) and bool(cr.strip())) or cr is True
            checks.append((named, "artifact: compatibility review recorded"
                           + (f" ({cr})" if isinstance(cr, str) and named else "" if named else
                              " — `change set --compat-reviewed <ref>` (the RFC section or review "
                              "that checked old peers / old data)")))
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
    evidence_note: str | None = None
    if eff_risk >= 2 and ci:
        # Local evidence.json is fabricatable, so it is NEVER the server-side gate of record. In CI the
        # required verify steps are separate REQUIRED JOBS (branch protection) that re-run from scratch.
        # This job cannot see their result, so it records no check here (a check that cannot fail is not
        # one, #887): it says where the evidence is enforced instead.
        evidence_note = ("not checked by this job — the required CI jobs (rust, spec-validate) re-run "
                         f"{', '.join(kspec.get('evidence', []) or ['verify'])}; local evidence.json is untrusted")
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
    render.field("Mode", "CI — diff + PR body (local work dir untrusted)" if ci else "local preflight")
    if evidence_note:
        render.field("Evidence", evidence_note)
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
