"""pr render — generate a PR body from the Change Contract, classification and evidence.

Pure rendering: it reads change.yaml, the classifier, impact.json and evidence.json and
emits Markdown (stdout or --out). It embeds the mandatory "AI assistance" disclosure block
from docs/contributing/ai-assisted-development.md so every PR carries it.
"""
from __future__ import annotations

import argparse
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from rusekit import repo, render, contract  # noqa: E402
from change.classify import classify_changeset  # noqa: E402

AI_BLOCK = """### AI assistance
- [ ] No significant AI assistance
- [ ] AI-assisted
  - Tool:            <tool / model, or "unspecified">
  - Scope:           <what the AI helped with — e.g. drafting tests, refactor, boilerplate>
  - Human verification performed: <how you verified — tests run, manual review, reasoning checked>"""


def _fmt_list(items, empty="_none_"):
    items = items or []
    return ", ".join(str(i) for i in items) if items else empty


GATE_MARKER = "ruse-gate:v1"


def _machine_block(c: dict) -> str:
    """A machine-readable gate block for CI. It carries the author's DECLARATION only — CI re-derives the
    observed kind + blast radius from the actual diff and re-runs verify, so this block is never trusted as
    evidence (`.ruse/` stays untrusted). Generated, not hand-authored (D-021 permits generated JSON)."""
    gate = {
        "v": 1,
        "kind": c.get("kind"),
        "goal": (c.get("goal") or "").strip(),
        "affected": c.get("affected") or {},
        "allow_paths": c.get("allow_paths") or [],
        "forbid_paths": c.get("forbid_paths") or [],
        "artifacts": c.get("artifacts") or {},
        "contracts": c.get("contracts") or {},
    }
    return (f"<!-- {GATE_MARKER} — author-declared merge-gate contract. CI re-derives observed kind + blast\n"
            f"     radius from the diff and re-runs verify; this block is NOT trusted as evidence. -->\n"
            "```json\n" + json.dumps(gate, indent=2, sort_keys=True) + "\n```")


def build(issue: str, base: str | None, files: list[str] | None) -> str:
    c = contract.load(issue) or {}
    cs = repo.resolve_changeset(base=base, files=files)
    if cs.source == "none":
        cs = repo.ChangeSet(files=contract.declared_paths(c), source="change-yaml")
    cl = classify_changeset(cs.files, c.get("kind"), source=cs.source)

    aff = c.get("affected") or {}
    art = c.get("artifacts") or {}
    con = c.get("contracts") or {}

    # evidence summary
    ev_path = os.path.join(repo.work_dir(issue), "evidence.json")
    ev_lines = []
    if os.path.isfile(ev_path):
        try:
            for cmd in (json.load(open(ev_path)) or {}).get("commands", []):
                mark = "✅" if cmd.get("exit_code") == 0 else "❌"
                dur = cmd.get("duration_ms")
                dur_s = f" ({dur} ms)" if dur is not None else ""
                ev_lines.append(f"- {mark} `{cmd.get('command', cmd.get('step',''))}`{dur_s}")
        except Exception:
            pass

    # impact summary
    ip_path = os.path.join(repo.work_dir(issue), "impact.json")
    impact_line = "_run `ruse impact --issue %s --out .ruse/work/%s/impact.json`_" % (issue, issue)
    if os.path.isfile(ip_path):
        try:
            ip = json.load(open(ip_path)) or {}
            roots = ip.get("roots", [])
            n = len(ip.get("reachable", {}))
            if roots:
                impact_line = f"Roots: {_fmt_list(roots)} · {n} nodes reachable"
        except Exception:
            pass

    contract_flags = [k for k, v in con.items() if v]

    L = []
    L.append(f"## {c.get('kind', 'change')}: {issue}")
    L.append("")
    L.append(c.get("goal", "").strip() or "_describe the change_")
    L.append("")
    L.append("### Change type")
    L.append(f"- Declared kind: **{c.get('kind', '—')}**")
    L.append(f"- Observed minimum: **{cl.observed_kind or 'none'}** "
             f"({'ok' if cl.ok else 'MISMATCH — raise the kind'})")
    if contract_flags:
        L.append(f"- Crosses contract boundary: **{_fmt_list(contract_flags)}**")
    L.append("")
    L.append("### Traceability")
    L.append(f"- Issue: {art.get('issue') or issue}")
    L.append(f"- RFC: {art.get('rfc') or '—'}")
    L.append(f"- Decision: {art.get('decision') or '—'}")
    L.append(f"- Capabilities: {_fmt_list(aff.get('capabilities'))}")
    L.append(f"- Requirements: {_fmt_list(aff.get('requirements'))}")
    L.append(f"- Invariants: {_fmt_list(aff.get('invariants'))}")
    L.append(f"- Crates: {_fmt_list(aff.get('crates'))}")
    L.append("")
    L.append("### Impact")
    L.append(impact_line)
    L.append("")
    L.append("### Compatibility")
    L.append(f"- Public API: {con.get('public_api', False)} · "
             f"Protocol: {con.get('protocol', False)} · "
             f"Persistent format: {con.get('persistent_format', False)} · "
             f"Migration: {art.get('migration', False)}")
    L.append("")
    L.append("### Evidence")
    L.extend(ev_lines or ["- _no recorded evidence — run `ruse verify` before merge_"])
    L.append("")
    L.append("### Non-goals")
    for ng in c.get("non_goals") or ["_none stated_"]:
        L.append(f"- {ng}")
    L.append("")
    L.append(AI_BLOCK)
    L.append("")
    L.append(_machine_block(c))
    L.append("")
    return "\n".join(L)


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(prog="ruse pr render")
    ap.add_argument("--issue", help="change workspace (default: active)")
    ap.add_argument("--base", help="git ref to diff against")
    ap.add_argument("--files", nargs="*", help="explicit file list")
    ap.add_argument("--out", help="write to this path instead of stdout")
    args = ap.parse_args(argv)

    issue = args.issue or repo.active_issue()
    if not issue:
        render.fail("no change workspace; run `ruse change start` first")
        return 1
    body = build(issue, args.base, args.files)
    if args.out:
        with open(repo.path(args.out), "w", encoding="utf-8") as fh:
            fh.write(body)
        render.field("Wrote", args.out)
    else:
        print(body)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
