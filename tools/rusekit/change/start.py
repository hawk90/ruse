"""change start — scaffold the local .ruse/work/<issue>/ change workspace.

Creates change.yaml (from spec/templates/change.yaml, pre-filled), plan.md, and empty
impact.json / evidence.json. Nothing here is committed — .ruse/ is gitignored; only the
eventual RFC/Decision/PRD/PR is permanent.
"""
from __future__ import annotations

import argparse
import json
import os
import sys

from rusekit import repo, render, contract  # noqa: E402


def _fill_change_yaml(template: str, issue: str, kind: str | None,
                      crates: list[str]) -> str:
    out = template
    out = out.replace('issue: "<id>"', f'issue: "{issue}"', 1)      # top-level (col 0)
    out = out.replace('  issue: "<id>"', f'  issue: "{issue}"', 1)  # artifacts.issue
    if kind:
        out = out.replace('kind: "<kind>"', f'kind: "{kind}"', 1)
    if crates:
        clist = ", ".join(crates)
        out = out.replace("  crates: []", f"  crates: [{clist}]", 1)
        paths = ", ".join(f'"crates/{c}/"' for c in crates)
        out = out.replace("allow_paths: []", f"allow_paths: [{paths}]", 1)
    return out


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(prog="ruse change start")
    ap.add_argument("--issue", required=True, help="tracking issue id / short slug")
    ap.add_argument("--kind", choices=contract.kind_names() or None,
                    help="declared change kind (see spec/change-kinds.yaml)")
    ap.add_argument("--area", "--crate", dest="area", action="append", default=[],
                    help="affected crate (repeatable): " + " | ".join(repo.CRATES))
    ap.add_argument("--goal", help="one-line goal (fills change.yaml)")
    ap.add_argument("--force", action="store_true", help="overwrite an existing workspace")
    args = ap.parse_args(argv)

    wd = repo.work_dir(args.issue)
    cy = os.path.join(wd, "change.yaml")
    if os.path.exists(cy) and not args.force:
        render.fail(f"{repo.rel(cy)} already exists (use --force to overwrite)")
        return 1

    bad = [a for a in args.area if a not in repo.CRATES]
    if bad:
        render.warn(f"unknown crate(s): {bad} (known: {repo.CRATES})")

    os.makedirs(wd, exist_ok=True)

    # change.yaml
    tmpl_path = repo.path("spec", "templates", "change.yaml")
    template = open(tmpl_path, encoding="utf-8").read() if os.path.isfile(tmpl_path) else ""
    text = _fill_change_yaml(template, args.issue, args.kind, args.area)
    if args.goal:
        text = text.replace(
            "  <one paragraph: the observable outcome this change delivers>",
            f"  {args.goal}", 1)
    with open(cy, "w", encoding="utf-8") as fh:
        fh.write(text)

    # plan.md
    plan_tmpl = repo.path("spec", "templates", "plan.md")
    plan_txt = open(plan_tmpl, encoding="utf-8").read() if os.path.isfile(plan_tmpl) else "# Plan\n"
    plan_txt = plan_txt.replace("<issue>", str(args.issue))
    with open(os.path.join(wd, "plan.md"), "w", encoding="utf-8") as fh:
        fh.write(plan_txt)

    # placeholders
    for name, payload in (("impact.json", {}), ("evidence.json", {"commands": []})):
        with open(os.path.join(wd, name), "w", encoding="utf-8") as fh:
            json.dump(payload, fh, indent=2)

    branch = repo.current_branch()

    render.heading("change start")
    render.field("Workspace", repo.rel(wd) + "/")
    render.field("Issue", str(args.issue))
    render.field("Kind", args.kind or "(unset — edit change.yaml)")
    render.field("Crates", ", ".join(args.area) or "(none)")
    render.field("Git branch", branch or "(not a git repo)")
    render.ok("workspace scaffolded")
    render.heading("\nNext")
    render.bullet(f"edit {repo.rel(cy)} (goal, non-goals, affected IDs, allow_paths)")
    render.bullet(f"python3 tools/ruse.py context build --issue {args.issue}")
    render.bullet(f"python3 tools/ruse.py impact --issue {args.issue}")
    render.bullet(f"python3 tools/ruse.py verify --changed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
