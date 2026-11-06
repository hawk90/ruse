#!/usr/bin/env python3
"""gov ratchet — tracked-debt buckets may never exceed their committed ceiling (turns "green" into progress).

Five debt ledgers are designed to keep the build green while their number "trends to zero": config keys with
`derives_from: []`, capabilities with an empty `prd:`, hand-authored parity IDs still cited by the PRD, the
parity backlog, and review axes that claim `method: machine` with no checker wired (`gov review_coverage`).
Nothing enforced the trend, so "all green" was a stable equilibrium at high debt, not a milestone
(architecture-review finding). This gate stores a ceiling per bucket in spec/parity/ratchet.yaml; a bucket
that RISES above its ceiling FAILS. Lower a ceiling when you burn the debt down and the gate holds the new
floor.

Raising a ceiling must say why (#884). The gate compares each ceiling with the base branch's ratchet.yaml (the
merge base with `--base`, else `origin/$GITHUB_BASE_REF`, else `origin/main`): a ceiling above the base's
FAILS unless `raised.<bucket>` in ratchet.yaml gives a reason that is new in this change (a reason already on
the base cannot excuse a second raise). A bucket new to the file may not start above its current count unless
it carries a reason the same way. With no base to compare (no git, no origin/main) the comparison is skipped
with a warning, except on a CI pull request (`GITHUB_BASE_REF` set), where a missing base FAILS.

`unclassified` census items are deliberately NOT ratcheted: they grow when a new upstream is censused
(discovery), which the census WANTS; partial classification is already blocked per-surface by
parity_discovery. Auto-discovered into `ruse gov check`.
"""
from __future__ import annotations

import argparse
import os
import sys

import yaml

from rusekit import render, repo  # noqa: E402
from rusekit.gov import parity_discovery, review_coverage  # noqa: E402

BASELINE = "spec/parity/ratchet.yaml"


def _load(path: str) -> dict:
    try:
        with open(repo.path(path), encoding="utf-8") as fh:
            return yaml.safe_load(fh) or {}
    except FileNotFoundError:
        return {}


def _review_unwired() -> int:
    axes = review_coverage.load_axes(_load(review_coverage.AXES))
    return len(review_coverage.evaluate(axes, review_coverage.valid_checkers())["gaps"])


def current_buckets() -> dict:
    settings = (_load("spec/config-schema.yaml").get("settings")) or {}
    config_untraced = sum(1 for v in settings.values()
                          if isinstance(v, dict) and v.get("derives_from") == [])
    caps = (_load("spec/capabilities.yaml").get("capabilities")) or {}
    cap_untraced = sum(1 for v in caps.values() if isinstance(v, dict) and not v.get("prd"))
    bl = _load("spec/parity-backlog.yaml")
    backlog = len(bl.get("items") or bl.get("backlog") or []) if isinstance(bl, dict) else 0
    legacy = len(parity_discovery.check().get("legacy_in_prd") or [])
    return {"config_untraced": config_untraced, "cap_untraced": cap_untraced,
            "backlog": backlog, "legacy_in_prd": legacy, "review_machine_unwired": _review_unwired()}


def base_ref(explicit: str | None) -> str | None:
    if explicit:
        return explicit
    env = (os.environ.get("GITHUB_BASE_REF") or "").strip()
    return f"origin/{env}" if env else "origin/main"


def base_baseline(ref: str) -> tuple[dict | None, str]:
    """The ratchet file as of the merge base with `ref`: (data, how). data None = could not be read;
    {} = the file does not exist there (nothing to compare)."""
    code, mb = repo._git("merge-base", "HEAD", ref)
    if code != 0 or not mb.strip():
        return None, f"no merge base with {ref}"
    rev = mb.strip()
    code, _ = repo._git("cat-file", "-e", f"{rev}:{BASELINE}")
    if code != 0:
        return {}, f"{BASELINE} absent at {rev[:10]}"
    code, text = repo._git("show", f"{rev}:{BASELINE}")
    if code != 0:
        return None, f"cannot read {BASELINE} at {rev[:10]}"
    try:
        return yaml.safe_load(text) or {}, f"merge base {rev[:10]} with {ref}"
    except yaml.YAMLError as e:
        return None, f"base {BASELINE} does not parse: {e}"


def unexplained_raises(base: dict, head: dict, current: dict) -> list[str]:
    """Buckets whose ceiling rose (vs base, or above the count for a new bucket) without a new reason."""
    bb, hb = base.get("buckets") or {}, head.get("buckets") or {}
    breasons, hreasons = base.get("raised") or {}, head.get("raised") or {}
    out = []
    for b, ceil in sorted(hb.items()):
        if not isinstance(ceil, (int, float)):
            continue
        before = bb.get(b)
        limit = before if isinstance(before, (int, float)) else current.get(b)
        if limit is None or ceil <= limit:
            continue
        why = hreasons.get(b)
        fresh = isinstance(why, str) and why.strip() and why != breasons.get(b)
        if not fresh:
            what = f"rose from {before} to {ceil}" if before is not None else \
                f"starts at {ceil}, above its count {limit}"
            out.append(f"{b}: ceiling {what} — say why in `raised.{b}` in {BASELINE} "
                       "(a new reason; one already on the base does not count)")
    return out


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(prog="gov ratchet")
    ap.add_argument("--base", help="git ref whose ratchet.yaml the ceilings are compared with "
                                   "(default: origin/$GITHUB_BASE_REF, else origin/main)")
    args = ap.parse_args(argv or [])
    head = _load(BASELINE)
    baseline = head.get("buckets") or {}
    current = current_buckets()

    render.heading("gov ratchet (tracked-debt ceilings)")
    if not baseline:
        render.fail(f"no baseline at {BASELINE} — create it with the current counts to arm the ratchet")
        for b, v in sorted(current.items()):
            render.field(b, str(v))
        return 1

    rose, tighten = [], []
    for b, cur in sorted(current.items()):
        ceil = baseline.get(b)
        render.field(b, f"{cur} / ceiling {ceil if ceil is not None else '—'}")
        if ceil is None:
            rose.append(f"{b}: {cur} has no declared ceiling — add one to {BASELINE}")
        elif cur > ceil:
            rose.append(f"{b} rose to {cur}, above ceiling {ceil} — trace/burn it down, or raise the "
                        f"ceiling with a reason in `raised.{b}`")
        elif cur < ceil:
            tighten.append(f"{b}: {cur} is below ceiling {ceil} — lower the ceiling to lock the gain")

    ref = base_ref(args.base)
    base, how = base_baseline(ref) if repo.is_git_repo() else (None, "not a git checkout")
    raises: list[str] = []
    if base is None:
        if os.environ.get("GITHUB_BASE_REF") or args.base:
            rose.append(f"cannot compare ceilings with the base ({how}) — a raised ceiling would go unseen")
        else:
            render.warn(f"raised-ceiling check skipped: {how} (pass --base)")
    else:
        render.field("compared with", how)
        raises = unexplained_raises(base, head, current)

    for r in rose + raises:
        render.bullet(r, mark="!")
    if rose or raises:
        render.fail(f"{len(rose) + len(raises)} ratchet finding(s): a bucket above its ceiling, "
                    "or a ceiling raised without a reason")
        return 1
    for t in tighten:
        render.warn(t)
    for b, why in sorted((head.get("raised") or {}).items()):
        render.field(f"raised {b}", str(why))
    render.ok("no tracked-debt bucket exceeds its ceiling; no ceiling rose without a reason")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
