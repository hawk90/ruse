#!/usr/bin/env python3
"""ruse — the single command-line entry point for the change workflow.

Contributors only need to remember this one command; everything else is an implementation
detail behind it (tools/change, tools/verify, tools/ai, tools/rusekit). Policy lives in
spec/ (change-kinds.yaml, PRD/POLICY/...), never in a Makefile or here.

  change start     scaffold .ruse/work/<issue>/ (Change Contract)
  change classify  compute the minimum required change-kind from the diff
  impact           what a spec ID or the diff ripples into
  context build    task-scoped context pack for an AI/human (+ staleness lock)
  context check    warn if the pack is stale
  plan validate    check a plan covers every required perspective
  verify           run only the checks the diff needs; record evidence
  spec validate    run spec-validate.py + change-workflow extensions
  spec generate    (P1) regenerate derived spec artifacts
  docs check       anchor / frontmatter / normative-leak hygiene
  arch deps        crate dependency contract (architecture.yaml) + cycles (ARCH-LAYER-001)
  gov check        run every governance checker (auto-discovered tools/gov/*.py)
  gov <checker>    one governance checker (e.g. gov waivers)
  phase sync       reconcile GitHub milestones from spec/phases.yaml (dry-run; --apply to mutate)
  pr render        generate the PR body from the contract + evidence
  pr check         the merge gate (classify + artifacts + blast radius + evidence)
  status           show the active change workspace
  bench            run the workspace benchmarks (--check compares to spec/perf-baseline.yaml)
"""
from __future__ import annotations

import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

from rusekit import repo, render, contract  # noqa: E402


def _usage(code: int = 0) -> int:
    print(__doc__.strip())
    return code


# ---- spec validate extension ----------------------------------------------------

REQUIRED_ARTIFACT_SET = {"issue", "rfc", "decision", "impact", "compat", "spec_ref",
                         "migration"}


def _validate_change_kinds() -> tuple[list[str], list[str]]:
    errs, warns = [], []
    policy = contract.load_kinds()
    if not policy:
        warns.append("spec/change-kinds.yaml missing or empty")
        return errs, warns
    kinds = policy.get("kinds") or {}
    seen_risk = {}
    for kid, k in kinds.items():
        r = k.get("risk")
        if not isinstance(r, int) or not (0 <= r <= 3):
            errs.append(f"change-kinds {kid}: risk must be int 0..3 (got {r})")
        seen_risk.setdefault(r, []).append(kid)
        for a in k.get("required_artifacts", []) or []:
            if a not in REQUIRED_ARTIFACT_SET:
                errs.append(f"change-kinds {kid}: unknown required_artifact '{a}'")
    for t in policy.get("path_triggers") or []:
        if t.get("min_kind") not in kinds:
            errs.append(f"change-kinds path_trigger: min_kind '{t.get('min_kind')}' undefined")
        try:
            re.compile(t.get("pattern", ""))
        except re.error as e:
            errs.append(f"change-kinds path_trigger pattern /{t.get('pattern')}/: {e}")
    for h in policy.get("human_judgment") or []:
        try:
            re.compile(h.get("pattern", ""))
        except re.error as e:
            errs.append(f"change-kinds human_judgment pattern: {e}")
    if not policy.get("generated_marker"):
        warns.append("change-kinds: no generated_marker defined")
    return errs, warns


def _scan_generated() -> list[str]:
    """Files carrying the generated marker (informational until D-022 generator exists)."""
    marker = contract.load_kinds().get("generated_marker")
    if not marker:
        return []
    hits = []
    for base in ("spec", "docs"):
        root = repo.path(base)
        for dirpath, _dirs, files in os.walk(root):
            for fn in files:
                if not fn.endswith((".md", ".yaml", ".yml")):
                    continue
                p = os.path.join(dirpath, fn)
                try:
                    with open(p, encoding="utf-8", errors="replace") as fh:
                        if marker in fh.read(2048):
                            hits.append(repo.rel(p))
                except OSError:
                    pass
    return hits


def cmd_spec(rest: list[str]) -> int:
    if not rest or rest[0] not in ("validate", "generate"):
        render.fail("usage: ruse spec validate | ruse spec generate")
        return 2
    if rest[0] == "generate":
        render.heading("spec generate")
        render.warn("the derived-artifact generator (D-022) is not built yet (P1).")
        render.bullet("planned: regenerate spec/CONTEXT.md, glossary.json, .context/ from sources")
        return 0

    # validate: run the reference checker first, then the change-workflow extensions.
    render.heading("spec validate")
    sv = repo.path("tools", "spec-validate.py")
    code = 0
    if os.path.isfile(sv):
        p = subprocess.run([sys.executable, sv], cwd=repo.ROOT)
        code = p.returncode
    else:
        render.warn("tools/spec-validate.py not found — skipping reference checks")

    print()
    render.heading("change-workflow extensions")
    errs, warns = _validate_change_kinds()
    gen = _scan_generated()
    render.field("Generated files", str(len(gen)))
    for g in gen:
        render.bullet(g, mark="·")
    for w in warns:
        render.warn(w)
    for e in errs:
        render.fail(e)
    if errs:
        code = 1
    if not errs:
        render.ok("change-kinds.yaml valid")
    print()
    render.result(code == 0, "spec validate: PASS" if code == 0 else "spec validate: FAIL")
    return code


def cmd_status(rest: list[str]) -> int:
    issue = repo.active_issue()
    render.heading("ruse status")
    render.field("Repo root", repo.ROOT)
    render.field("Git branch", repo.current_branch() or "(not a git repo)")
    if not issue:
        render.warn("no active change workspace — run `ruse change start --issue <id>`")
        return 0
    c = contract.load(issue) or {}
    wd = repo.work_dir(issue)
    render.field("Active issue", str(issue))
    render.field("Kind", c.get("kind") or "(unset)")
    render.field("Goal", (c.get("goal") or "").strip().splitlines()[0] if c.get("goal") else "—")
    for name in ("change.yaml", "plan.md", "context.md", "context-lock.json",
                 "impact.json", "evidence.json"):
        mark = "✓" if os.path.isfile(os.path.join(wd, name)) else "·"
        render.bullet(f"{mark} {name}")
    return 0


# ---- dispatch -------------------------------------------------------------------

def main() -> int:
    argv = sys.argv[1:]
    if not argv or argv[0] in ("-h", "--help", "help"):
        return _usage(0)
    cmd, rest = argv[0], argv[1:]

    if cmd == "change":
        if not rest:
            render.fail("usage: ruse change start|classify ...")
            return 2
        sub, subrest = rest[0], rest[1:]
        if sub == "start":
            from rusekit.change import start
            return start.main(subrest)
        if sub == "classify":
            from rusekit.change import classify
            return classify.main(subrest)
        render.fail(f"unknown: change {sub}")
        return 2

    if cmd == "impact":
        from rusekit.change import impact
        return impact.main(rest)

    if cmd == "context":
        from rusekit.ai import context_pack
        return context_pack.main(rest)

    if cmd == "plan":
        if not rest or rest[0] != "validate":
            render.fail("usage: ruse plan validate [path]")
            return 2
        from rusekit.ai import plan_validate
        return plan_validate.main(rest[1:])

    if cmd == "verify":
        from rusekit.verify import run
        return run.main(rest)

    if cmd == "spec":
        return cmd_spec(rest)

    if cmd == "docs":
        if not rest or rest[0] != "check":
            render.fail("usage: ruse docs check")
            return 2
        from rusekit.docs import check
        return check.main(rest[1:])

    if cmd == "arch":
        if not rest or rest[0] != "deps":
            render.fail("usage: ruse arch deps")
            return 2
        from rusekit.arch import dependencies
        return dependencies.main(rest[1:])

    if cmd == "phase":
        if not rest or rest[0] != "sync":
            render.fail("usage: ruse phase sync [--apply] [--prune]")
            return 2
        import phase_sync
        return phase_sync.main(rest[1:])

    if cmd == "gov":
        # Auto-discover tools/gov/*.py checkers — adding one needs NO edit here.
        import glob
        import importlib
        names = sorted(os.path.splitext(os.path.basename(p))[0]
                       for p in glob.glob(os.path.join(HERE, "rusekit", "gov", "*.py"))
                       if not p.endswith("__init__.py"))
        if not rest or rest[0] not in (*names, "check"):
            render.fail(f"usage: ruse gov <{'|'.join(names)}|check>")
            return 2
        if rest[0] == "check":
            render.heading("gov check")
            render.bullet("checkers: " + ", ".join(names))
            print()
            rc = 0
            for n in names:
                rc |= (importlib.import_module(f"rusekit.gov.{n}").main([]) or 0)
            return rc
        return importlib.import_module(f"rusekit.gov.{rest[0]}").main(rest[1:])

    if cmd == "pr":
        if not rest or rest[0] not in ("render", "check"):
            render.fail("usage: ruse pr render|check ...")
            return 2
        sub, subrest = rest[0], rest[1:]
        if sub == "render":
            from rusekit.change import pr_render
            return pr_render.main(subrest)
        from rusekit.change import pr_check
        return pr_check.main(subrest)

    if cmd == "status":
        return cmd_status(rest)

    if cmd == "bench":
        from rusekit import bench
        return bench.main(rest)

    render.fail(f"unknown command: {cmd}")
    return _usage(2)


if __name__ == "__main__":
    raise SystemExit(main())
