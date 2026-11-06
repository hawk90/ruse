"""verify — run only the checks the diff needs, and record real evidence.

`--changed` picks steps from the changed-file scope; `--full` runs everything applicable.
A model's "tests passed" claim is not evidence (ai-assisted-development.md) — so every step
here records its actual command, exit code and duration into $(git rev-parse --git-path ruse/work)/<id>/evidence.json,
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

from rusekit import repo, render  # noqa: E402

# Steps come from the ONE registry (spec/verification.yaml) so verify, lefthook and CI cannot drift.
#   scope: "spec_docs" | "rust"  · needs: tool that must exist · heavy: excluded from --changed
#   gate_on: a file that must exist or the step is skipped.  "python3" in cmd → this interpreter.
REGISTRY = "spec/verification.yaml"


class RegistryError(Exception):
    """spec/verification.yaml is missing, unreadable or malformed. verify then FAILS: falling back to a
    subset would report a pass for steps it never ran (#888)."""


def _load_steps(path: str | None = None) -> list[dict]:
    py = sys.executable
    path = path or repo.path(REGISTRY)
    try:
        import yaml
    except ImportError as e:
        raise RegistryError(f"PyYAML is required to read {REGISTRY} (`pip install pyyaml`)") from e
    try:
        with open(path, encoding="utf-8") as fh:
            data = yaml.safe_load(fh) or {}
    except (OSError, yaml.YAMLError) as e:
        raise RegistryError(f"cannot read {REGISTRY}: {e}") from e
    raw = data.get("steps") if isinstance(data, dict) else None
    if not isinstance(raw, list) or not raw:
        raise RegistryError(f"{REGISTRY} has no `steps:` list")
    steps = []
    for i, s in enumerate(raw):
        if not isinstance(s, dict) or not s.get("name") or not isinstance(s.get("cmd"), list) or not s["cmd"]:
            raise RegistryError(f"{REGISTRY} step #{i + 1} needs a `name` and a non-empty `cmd` list: {s!r}")
        scope = s.get("scope", "spec_docs")
        if scope not in SCOPES:
            raise RegistryError(f"{REGISTRY} step {s['name']}: scope must be one of {sorted(SCOPES)}")
        steps.append({"name": s["name"], "scope": scope,
                      "needs": s.get("needs", "python3"), "heavy": bool(s.get("heavy")),
                      "gate_on": s.get("gate_on"),
                      "cmd": [py if c == "python3" else c for c in s["cmd"]]})
    return steps


SCOPES = {"spec_docs", "rust"}

# tools the change-kinds policy names but whose implementation is not built yet.
NOT_YET = {"public-api-diff", "protocol-compat"}

# Paths outside crates/apps/tools that still change what the Rust build, lint or tests do.
RUST_CONFIG = ("Cargo.toml", "Cargo.lock", "clippy.toml", "rustfmt.toml", ".rustfmt.toml",
               "rust-toolchain.toml", "rust-toolchain")
RUST_DIRS = ("crates/", "apps/", "tools/", "tests/", "fuzz/", ".cargo/")


def _scope(files: list[str], full: bool) -> set[str]:
    """The scopes a diff needs. Fail safe (#888): a file no rule recognises selects every scope."""
    if full:
        return set(SCOPES)
    s: set[str] = set()
    for f in files:
        hit = False
        if f.startswith(("spec/", "docs/")) or f.endswith((".md", ".yaml", ".yml")):
            s.add("spec_docs")
            hit = True
        if f.startswith(RUST_DIRS) or f.endswith(".rs") or f.rsplit("/", 1)[-1] in RUST_CONFIG:
            s.add("rust")
            hit = True
        if not hit:
            s |= SCOPES
    return s


def _select(steps: list[dict], scopes: set[str], heavy: bool) -> tuple[list[dict], list[str], list[str]]:
    """(selected, skipped by design, cannot run). A step the diff needs whose tool is missing cannot be
    verified, so it is a failure, not a skip (#888)."""
    selected, skipped, missing = [], [], []
    for st in steps:
        if st["scope"] not in scopes:
            continue
        if st.get("heavy") and not heavy:
            skipped.append(f"{st['name']} (heavy; use --heavy or --full)")
            continue
        if st.get("gate_on") and not os.path.isfile(repo.path(st["gate_on"])):
            skipped.append(f"{st['name']} (gate {st['gate_on']} absent)")
            continue
        if st.get("needs") and st["needs"] != "python3" and shutil.which(st["needs"]) is None:
            missing.append(f"{st['name']} (missing tool: {st['needs']})")
            continue
        selected.append(st)
    return selected, skipped, missing


TEST_REPORT_LINES = 120   # at most this much of cargo test's failure reports is printed
STDERR_HEAD_LINES = 40    # and of stderr's first diagnostic (a clippy or compile error)
STDERR_TAIL_LINES = 8


def failure_excerpt(out: str, err: str) -> list[str]:
    """What a failed step prints (#1225): enough to see WHY without re-running it.

    `cargo test` writes each failing test's captured output, panic message included, to STDOUT
    (`---- <name> stdout ----` blocks up to the closing `failures:` list) and only "error: test
    failed" to stderr, so a stderr tail alone loses the message. Clippy and rustc write the
    diagnostic first and a summary last, so stderr's first `error`/`warning` block is kept too.
    """
    lines: list[str] = []
    report: list[str] = []
    inside = False
    for ln in out.splitlines():
        if ln.startswith("---- ") and ln.endswith(" ----"):
            inside = True
        elif inside and ln == "failures:":
            inside = False
        if inside:
            report.append(ln)
    if report:
        lines += report[:TEST_REPORT_LINES]
        if len(report) > TEST_REPORT_LINES:
            lines.append(f"... ({len(report) - TEST_REPORT_LINES} more lines of test output)")
    err_lines = err.splitlines()
    first = next((i for i, ln in enumerate(err_lines)
                  if ln.startswith(("error", "warning"))), None)
    tail_from = max(0, len(err_lines) - STDERR_TAIL_LINES)
    if first is not None and first < tail_from:
        head_end = min(first + STDERR_HEAD_LINES, tail_from)
        lines += err_lines[first:head_end]
        if head_end < tail_from:
            lines.append("...")
    lines += err_lines[tail_from:]
    return lines


def _run_step(st: dict) -> dict:
    t0 = time.time()
    try:
        p = subprocess.run(st["cmd"], cwd=repo.ROOT, capture_output=True, text=True,
                           timeout=600)
        code, out, errtxt = p.returncode, p.stdout, p.stderr
    except subprocess.TimeoutExpired as e:
        partial = e.stdout.decode(errors="replace") if isinstance(e.stdout, bytes) else (e.stdout or "")
        code, out, errtxt = 124, partial, "timed out after 600s"
    except FileNotFoundError as e:
        code, out, errtxt = 127, "", str(e)
    dur = int((time.time() - t0) * 1000)
    return {"step": st["name"], "command": " ".join(st["cmd"]),
            "exit_code": code, "duration_ms": dur,
            "stderr_tail": "\n".join(errtxt.splitlines()[-STDERR_TAIL_LINES:]),
            "_excerpt": failure_excerpt(out, errtxt) if code != 0 else []}


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

    try:
        steps = _load_steps()
    except RegistryError as e:
        render.fail(f"verify: {e}")
        return 2
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
    selected, skipped, missing = _select(steps, scopes, heavy)

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
        for s in missing:
            render.fail(f"cannot run {s}")
        for nm in sorted(NOT_YET):
            render.bullet(f"{nm}: not built yet (P1)", mark="·")
        return 0

    render.heading("\nRunning")
    results = []
    passed = True
    for st in selected:
        r = _run_step(st)
        excerpt = r.pop("_excerpt")   # printed, not recorded: evidence keeps the short stderr tail
        results.append(r)
        good = r["exit_code"] == 0
        passed = passed and good
        line = f"{st['name']:<16} exit={r['exit_code']} {r['duration_ms']}ms"
        (render.ok if good else render.fail)(line)
        for ln in excerpt:
            print("      " + render.c(ln, "dim"))
    for s in skipped:
        render.warn(f"skipped {s}")
    for s in missing:
        render.fail(f"not run {s} — install it; a step the change needs is never skipped silently")
        passed = False

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
        render.field("\nEvidence", repo.show(ep))

    print()
    render.result(passed, "verify: all selected checks passed" if passed
                  else "verify: one or more checks failed")
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
