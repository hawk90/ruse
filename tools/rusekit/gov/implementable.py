#!/usr/bin/env python3
"""gov implementable — is a feature's vertical chain closed enough to start implementing it?

Operationalizes the rule the parity/decision layers imply: a feature is ready to implement when its evidence
chain is closed AND the kernel decisions its components sit on are resolved. Chain closure alone is NOT enough
— a feature can have parity->screen->PRD->CFG all green while sitting on an OPEN kernel decision, and
implementing then bakes the wrong primitive (why D-047 had to land before input.rs op-pending).

Per feature F:
  parity     trace.parity points at CENSUS ids (nvim./emacs./helix.*), not legacy VIM-*/COM-*...; absent
             trace.parity is treated as N/A (infra features legitimately have no upstream target).
  screened   each census ref is classified into a family (status targeted/committed + a `family:` tag).
  cfg        config keys that derive_from F (informational — a feature may legitimately own none).
  decisions  no irreconcilable concept with resolution=pending blocks any of F.depends_on components.

The decision check is component-granular (conservative): a pending concept blocks a component's ASPECT, but
this gate cannot see whether F touches that aspect. A feature that genuinely does not may declare an
`implementable_ack: [{concept: CONCEPT-X, reason: ...}]`. An ack is honoured ONLY if (a) it carries a reason
and (b) CONCEPT-X actually couples one of F's depends_on components — you cannot ack a coupling that is not
there, and every honoured ack is REPORTED, so this is an audited engineering judgment, not a silent escape
hatch. A reasonless or spurious ack is ignored and WARNed.

Verdict: READY (chain closed, no unacked pending blocker) / READY-WITH-SCOPE (chain closed, but an unacked
pending concept blocks a component — carve that sub-part out) / NOT-READY (parity legacy-only or unscreened).

`ruse gov implementable <F-id>` details one feature; `--all` lists every mvp feature; no arg gives a terse
summary (advisory, never fails the build) and WARNs on a `status: active` feature that is not READY.
Auto-discovered into `ruse gov check`.
"""
from __future__ import annotations

import argparse
import glob
import re
import sys

import yaml

from rusekit import render, repo  # noqa: E402

PRD = "spec/PRD.yaml"
CFG = "spec/config-schema.yaml"
IRRECON = "spec/parity/concepts/irreconcilable.yaml"
INV_GLOB = "spec/parity/inventory/*/*.yaml"
LEGACY_RE = re.compile(r"^(?:VIM|NVIM|EMACS|COM|TERM|WS|REM|ECO|NAT)-")
SCREENED_STATUS = {"targeted", "intentionally-different"}


def _load(path: str) -> dict:
    try:
        with open(repo.path(path), encoding="utf-8") as fh:
            return yaml.safe_load(fh) or {}
    except FileNotFoundError:
        return {}


def census_index() -> dict:
    idx = {}
    for path in glob.glob(repo.path(INV_GLOB)):
        for it in (yaml.safe_load(open(path, encoding="utf-8")) or {}).get("items") or []:
            if it.get("id"):
                idx[it["id"]] = (it.get("status", "unclassified"), it.get("family"))
    return idx


def pending_blockers() -> dict:
    out: dict[str, list[str]] = {}
    for cid, body in (_load(IRRECON).get("concepts") or {}).items():
        body = body or {}
        if body.get("resolution") == "pending":
            for comp in (body.get("blocks") or []):
                out.setdefault(comp, []).append(cid)
    return out


def config_keys_for(fid: str, settings: dict) -> list[str]:
    return [k for k, v in settings.items()
            if isinstance(v, dict) and fid in (v.get("derives_from") or [])]


def assess(fid: str, feat: dict, cidx: dict, blockers: dict, settings: dict) -> dict:
    par = ((feat.get("trace") or {}).get("parity")) or []
    census = [p for p in par if p in cidx]
    legacy = [p for p in par if LEGACY_RE.match(p)]
    unknown = [p for p in par if p not in cidx and not LEGACY_RE.match(p)]
    unscreened = [p for p in census if cidx[p][0] not in SCREENED_STATUS or not cidx[p][1]]

    dep = feat.get("depends_on") or []
    coupled = {c: blockers[c] for c in dep if c in blockers}          # component -> [concept ids]
    coupled_concepts = {con for cons in coupled.values() for con in cons}

    ack_reason, invalid_acks = {}, []
    for a in (feat.get("implementable_ack") or []):
        con, reason = (a or {}).get("concept"), (a or {}).get("reason")
        if con and reason:
            ack_reason[con] = reason
        else:
            invalid_acks.append(a)
    spurious_acks = sorted(c for c in ack_reason if c not in coupled_concepts)

    blocked_real, blocked_acked = {}, {}
    for comp, cons in coupled.items():
        unacked = [c for c in cons if c not in ack_reason]
        acked = [c for c in cons if c in ack_reason]
        if unacked:
            blocked_real[comp] = unacked
        if acked:
            blocked_acked[comp] = acked

    parity_na = not par
    chain_closed = parity_na or (census and not legacy and not unscreened and not unknown)
    verdict = ("NOT-READY" if not chain_closed
               else "READY-WITH-SCOPE" if blocked_real
               else "READY")
    return {"verdict": verdict, "parity_na": parity_na, "census": census, "legacy": legacy,
            "unknown": unknown, "unscreened": unscreened, "blocked_real": blocked_real,
            "blocked_acked": blocked_acked, "ack_reason": ack_reason, "spurious_acks": spurious_acks,
            "invalid_acks": invalid_acks, "cfg": config_keys_for(fid, settings),
            "status": feat.get("status"), "stage": feat.get("stage"), "title": feat.get("title")}


def _warn_acks(a: dict, fid: str) -> None:
    for c in a["spurious_acks"]:
        render.warn(f"{fid}: implementable_ack cites {c} but it blocks no depends_on component — stale ack?")
    if a["invalid_acks"]:
        render.warn(f"{fid}: {len(a['invalid_acks'])} implementable_ack entr(y/ies) missing concept/reason — ignored")


def detail(fid: str, a: dict) -> None:
    render.heading(f"gov implementable — {fid}: {a['title']}")
    render.field("Verdict", a["verdict"])
    render.field("Stage/status", f"{a['stage']} / {a['status']}")
    render.field("Parity", "N/A (no upstream target)" if a["parity_na"]
                 else f"{len(a['census'])} census, {len(a['legacy'])} legacy, {len(a['unscreened'])} unscreened")
    if a["legacy"]:
        render.bullet(f"legacy parity ids still cited (repoint to census): {', '.join(a['legacy'])}", mark="!")
    if a["unscreened"]:
        render.bullet(f"census ids not yet screened into a family: {', '.join(a['unscreened'])}", mark="!")
    if a["unknown"]:
        render.bullet(f"parity ids neither census nor legacy (typo?): {', '.join(a['unknown'])}", mark="!")
    render.field("Config keys", ", ".join(a["cfg"]) or "(none)")
    for comp, cons in a["blocked_real"].items():
        render.bullet(f"{comp} blocked by pending {', '.join(cons)} — carve out the sub-part it governs",
                      mark="!")
    for comp, cons in a["blocked_acked"].items():
        for c in cons:
            render.field(f"ack {comp}", f"{c} — {a['ack_reason'][c]}")
    if not a["blocked_real"] and not a["blocked_acked"]:
        render.field("Coupled decisions", "all depends_on components decided")
    _warn_acks(a, fid)

    if a["verdict"] == "READY":
        render.ok("chain closed, no unacked pending decision blocks a dependency — implement")
    elif a["verdict"] == "READY-WITH-SCOPE":
        render.warn("chain closed; implement everything EXCEPT the sub-parts governed by the pending concepts")
    else:
        render.fail("chain not closed — screen the census / repoint legacy before implementing")


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(prog="gov implementable")
    ap.add_argument("feature", nargs="?", help="feature id, e.g. F-003; omit for an all-mvp summary")
    ap.add_argument("--all", action="store_true", help="list every mvp feature's verdict")
    args = ap.parse_args(argv or [])

    feats = _load(PRD).get("features") or {}
    settings = _load(CFG).get("settings") or {}
    cidx, blockers = census_index(), pending_blockers()

    if args.feature:
        f = feats.get(args.feature)
        if not f:
            render.fail(f"no such feature {args.feature}")
            return 1
        detail(args.feature, assess(args.feature, f, cidx, blockers, settings))
        return 0

    render.heading("gov implementable (mvp feature readiness)")
    counts = {"READY": 0, "READY-WITH-SCOPE": 0, "NOT-READY": 0}
    active_not_ready, acks_total = [], 0
    for fid, f in sorted(feats.items()):
        if f.get("stage") != "mvp":
            continue
        a = assess(fid, f, cidx, blockers, settings)
        counts[a["verdict"]] += 1
        acks_total += sum(len(v) for v in a["blocked_acked"].values())
        if args.all:
            tag = {"READY": None, "READY-WITH-SCOPE": "~", "NOT-READY": "!"}[a["verdict"]]
            render.bullet(f"{fid}: {a['verdict']}", mark=tag)
        if a["status"] == "active" and a["verdict"] != "READY":
            active_not_ready.append(fid)
        _warn_acks(a, fid)
    render.field("Summary", f"READY {counts['READY']} · WITH-SCOPE {counts['READY-WITH-SCOPE']} · "
                            f"NOT-READY {counts['NOT-READY']}  (acked couplings: {acks_total})")
    for fid in active_not_ready:
        render.warn(f"{fid} is status:active but not READY — run `ruse gov implementable {fid}`")
    render.ok("advisory readiness computed (never blocks the build)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
