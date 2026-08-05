"""verify — run only the checks the diff needs, and record real evidence.

`--changed` picks steps from the changed-file scope; `--full` runs everything applicable.
A model's "tests passed" claim is not evidence (ai-assisted-development.md) — so every step
here records its actual command, exit code and duration into .ruse/work/<id>/evidence.json,
which `pr check` and `pr render` consume.
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from rusekit import repo, render  # noqa: E402

# Each step: scope predicate decides inclusion in --changed; --full runs all applicable.
#   scope: "spec_docs" | "rust"      needs: external tool that must exist
#   heavy: excluded from --changed unless --heavy (kept out of the fast inner loop)
STEPS = [
    {"name": "spec-validate", "scope": "spec_docs", "needs": "python3",
     "cmd": [sys.executable, "tools/spec-validate.py"]},
    {"name": "docs-check", "scope": "spec_docs", "needs": "python3",
     "cmd": [sys.executable, "tools/docs/check.py"]},
    {"name": "dependency-check", "scope": "rust", "needs": "cargo",
     "cmd": [sys.executable, "tools/arch/dependencies.py"]},
    {"name": "cargo-fmt", "scope": "rust", "needs": "cargo",
     "cmd": ["cargo", "fmt", "--check"]},
    {"name": "cargo-clippy", "scope": "rust", "needs": "cargo", "heavy": True,
     "cmd": ["cargo", "clippy", "--workspace", "--all-targets", "-q"]},
    {"name": "cargo-test", "scope": "rust", "needs": "cargo", "heavy": True,
     "cmd": ["cargo", "test", "--workspace", "-q"]},
]

# tools the change-kinds policy names but whose implementation is not built yet.
NOT_YET = {"public-api-diff", "protocol-compat"}


def _scope(files: list[str], full: bool) -> set[str]:
    if full:
        return {"spec_docs", "rust"}
    s: set[str] = set()
    for f in files:
        if f.startswith(("spec/", "docs/")) or f.endswith((".md", ".yaml", ".yml")):
            s.add("spec_docs")
        if f.startswith(("crates/", "apps/", "tools/")) or f.endswith(".rs"):
            s.add("rust")
    return s


def _select(scopes: set[str], heavy: bool) -> tuple[list[dict], list[str]]:
    selected, skipped = [], []
    for st in STEPS:
        if st["scope"] not in scopes:
            continue
        if st.get("heavy") and not heavy:
            skipped.append(f"{st['name']} (heavy; use --heavy or --full)")
            continue
        if st.get("needs") and shutil.which(st["needs"]) is None and st["needs"] != "python3":
            skipped.append(f"{st['name']} (missing tool: {st['needs']})")
            continue
        selected.append(st)
    return selected, skipped


def _run_step(st: dict) -> dict:
    t0 = time.time()
    try:
        p = subprocess.run(st["cmd"], cwd=repo.ROOT, capture_output=True, text=True,
                           timeout=600)
        code, out, errtxt = p.returncode, p.stdout, p.stderr
    except subprocess.TimeoutExpired:
        code, out, errtxt = 124, "", "timed out after 600s"
    except FileNotFoundError as e:
        code, out, errtxt = 127, "", str(e)
    dur = int((time.time() - t0) * 1000)
    return {"step": st["name"], "command": " ".join(st["cmd"]),
            "exit_code": code, "duration_ms": dur,
            "stderr_tail": "\n".join(errtxt.splitlines()[-8:])}


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(prog="ruse verify")
    g = ap.add_mutually_exclusive_group()
    g.add_argument("--changed", action="store_true", help="only checks the diff needs (default)")
    g.add_argument("--full", action="store_true", help="run every applicable check")
    ap.add_argument("--files", nargs="*", help="explicit file list")
    ap.add_argument("--base", help="git ref to diff against")
    ap.add_argument("--heavy", action="store_true", help="include cargo clippy/test in --changed")
    ap.add_argument("--issue", help="record evidence into this workspace (default: active)")
    ap.add_argument("--no-record", action="store_true", help="do not write evidence.json")
    ap.add_argument("--list", action="store_true", help="show selected steps, do not run")
    args = ap.parse_args(argv)

    cs = repo.resolve_changeset(base=args.base, files=args.files)
    if args.full:
        scopes = {"spec_docs", "rust"}
    elif cs.source == "none" and not args.files:
        scopes = {"spec_docs"}   # no git: default to the always-safe spec check
        render.warn("no git/--files: defaulting to spec_docs scope. Use --full to force all.")
    else:
        scopes = _scope(cs.files, full=False)
        if not scopes:
            scopes = {"spec_docs"}

    heavy = args.heavy or args.full
    selected, skipped = _select(scopes, heavy)

    render.heading("verify")
    render.field("Mode", "full" if args.full else "changed")
    render.field("Scope", ", ".join(sorted(scopes)) or "(none)")
    render.field("Changed files", f"{len(cs.files)} (source: {cs.source})")

    if args.list:
        render.heading("\nWould run")
        for st in selected:
            render.bullet(f"{st['name']}: {' '.join(st['cmd'])}")
        for s in skipped:
            render.warn(f"skip {s}")
        for nm in sorted(NOT_YET):
            render.bullet(f"{nm}: not built yet (P1)", mark="·")
        return 0

    render.heading("\nRunning")
    results = []
    passed = True
    for st in selected:
        r = _run_step(st)
        results.append(r)
        good = r["exit_code"] == 0
        passed = passed and good
        line = f"{st['name']:<16} exit={r['exit_code']} {r['duration_ms']}ms"
        (render.ok if good else render.fail)(line)
        if not good and r["stderr_tail"]:
            for ln in r["stderr_tail"].splitlines():
                print("      " + render.c(ln, "dim"))
    for s in skipped:
        render.warn(f"skipped {s}")

    # record evidence
    issue = args.issue or repo.active_issue()
    if results and issue and not args.no_record:
        commit = None
        if repo.is_git_repo():
            code, out = repo._git("rev-parse", "--short", "HEAD")
            commit = out.strip() if code == 0 else None
        payload = {"commit": commit, "scope": sorted(scopes),
                   "mode": "full" if args.full else "changed", "commands": results}
        ep = os.path.join(repo.work_dir(issue), "evidence.json")
        os.makedirs(os.path.dirname(ep), exist_ok=True)
        with open(ep, "w", encoding="utf-8") as fh:
            json.dump(payload, fh, indent=2)
        render.field("\nEvidence", repo.rel(ep))

    print()
    render.result(passed, "verify: all selected checks passed" if passed
                  else "verify: one or more checks failed")
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
