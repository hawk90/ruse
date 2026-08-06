"""context build / context check — a task-scoped context pack for an AI (or human).

Instead of feeding a model the whole repo, gather only what the change touches: goal,
non-goals, the related spec IDs (expanded one hop over the impact graph), the source files,
the allowed/forbidden blast radius, and the verify + done conditions. It also writes a
context-lock.json of source hashes so `context check` can warn when a spec changed under a
stale pack ("rebuild before continuing").
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from rusekit import repo, render, contract, model as model_mod  # noqa: E402

KIND_SECTION = [
    ("CAP", "Capabilities"), ("F", "Requirements (features)"),
    ("C", "Requirements (components)"), ("INV", "Invariants"),
    ("ENG", "Principles"), ("D", "Decisions"), ("ARCH", "Architecture"),
    ("RFC", "RFCs"), ("DEP", "Dependencies"), ("DOC", "Design docs"),
    ("CRATE", "Crates"),
]


def _sha(relfile: str) -> str | None:
    p = repo.path(relfile)
    if not os.path.isfile(p):
        return None
    h = hashlib.sha256()
    with open(p, "rb") as fh:
        for chunk in iter(lambda: fh.read(65536), b""):
            h.update(chunk)
    return "sha256:" + h.hexdigest()


def _profiles_doc() -> dict:
    import yaml
    p = repo.path("spec/context-profiles.yaml")
    return (yaml.safe_load(open(p)) or {}) if os.path.isfile(p) else {}


def _always_include() -> list[str]:
    """Docs every pack must start with (context-profiles.yaml `always_include`)."""
    return list(_profiles_doc().get("always_include") or [])


def _resolve_profile(name: str) -> tuple[list[str], list[str]]:
    """Return (ids, doc_paths) from spec/context-profiles.yaml."""
    p = (_profiles_doc().get("profiles") or {}).get(name)
    if not p:
        return [], []
    ids, docs = [], []
    for inc in p.get("include") or []:
        if "/" in inc or inc.endswith(".md"):
            docs.append(inc)
        else:
            ids.append(inc)
    return ids, docs


def build(issue: str | None, roots: list[str], depth: int,
          extra_docs: list[str]) -> tuple[str, dict]:
    m = model_mod.load()
    c = contract.load(issue) if issue else None

    # Unknown roots are an error — never silently drop what the caller asked for.
    missing = [r for r in roots if not m.has(r)]
    if missing:
        raise ValueError(f"unknown spec ID(s): {', '.join(sorted(missing))}")

    # Every pack starts with always_include (spec/context-profiles.yaml).
    extra_docs = list(extra_docs) + _always_include()

    reach = m.bfs(roots, direction="both", depth=depth)
    included = sorted(reach.keys(), key=lambda i: (reach[i], i))

    # group ids by kind
    by_kind: dict[str, list[str]] = {}
    for nid in included:
        by_kind.setdefault(m.nodes[nid].kind, []).append(nid)

    # source files backing the pack (dedup) → hashed for the lock
    source_files: dict[str, str | None] = {}
    for nid in included:
        f = m.nodes[nid].file
        if f and not os.path.isdir(repo.path(f)):
            source_files.setdefault(f, None)
    # A doc include may carry a `#anchor` naming the section to focus on (e.g. an
    # anti-patterns category or an architecture.md subsection). Hash the whole file, but
    # keep the anchor(s) as a reading hint so the pack points at the relevant slice.
    doc_sections: dict[str, list[str]] = {}
    for d in extra_docs:
        base, _, anchor = d.partition("#")
        rel = base if base.startswith(("docs/", "spec/")) \
            else os.path.normpath(os.path.join("spec", base)).replace(os.sep, "/")
        source_files.setdefault(rel, None)
        if anchor and anchor not in doc_sections.setdefault(rel, []):
            doc_sections[rel].append(anchor)
    for f in source_files:
        source_files[f] = _sha(f)

    # crates / blast radius / evidence from the contract
    affected = (c or {}).get("affected", {}) or {}
    crates = affected.get("crates") or []
    allow = (c or {}).get("allow_paths") or [f"crates/{cc}/" for cc in crates]
    forbid = (c or {}).get("forbid_paths") or []
    kind = (c or {}).get("kind")
    kspec = (contract.load_kinds().get("kinds") or {}).get(kind, {})
    evidence = kspec.get("evidence", ["spec-validate"])

    # ---- render markdown ----
    # Canonical generated marker (spec/change-kinds.yaml generated_marker) so every tool
    # recognises this file as generated — do not diverge the wording per generator.
    L = ["<!-- GENERATED FILE: DO NOT EDIT -->",
         "<!-- generator: ruse context build · source-version: 1 · rebuild instead of editing -->"]
    L.append(f"# Context pack — {issue or 'ad-hoc'}")
    L.append("")
    if c:
        L.append(f"**Kind:** {kind or '—'}")
        L.append("")
        L.append("## Goal")
        L.append((c.get("goal") or "").strip() or "_unset_")
        L.append("")
        L.append("## Non-goals")
        for ng in c.get("non_goals") or ["_none stated_"]:
            L.append(f"- {ng}")
        L.append("")
    L.append("## In-scope spec IDs")
    for kind_key, heading in KIND_SECTION:
        ids = sorted(by_kind.get(kind_key, []))
        if ids:
            L.append(f"### {heading}")
            for nid in ids:
                dist = reach[nid]
                tag = " *(root)*" if dist == 0 else f" *(+{dist})*"
                L.append(f"- `{nid}`{tag} — {m.title(nid)}".rstrip())
            L.append("")
    L.append("## Source files (read these; hashed in the lock)")
    for f in sorted(source_files):
        secs = doc_sections.get(f)
        focus = f"  — focus: {', '.join('#' + s for s in secs)}" if secs else ""
        L.append(f"- `{f}`{focus}")
    L.append("")
    L.append("## Blast radius")
    L.append(f"- **Allowed paths:** {', '.join(allow) if allow else '_unset — set allow_paths_'}")
    L.append(f"- **Forbidden paths:** {', '.join(forbid) if forbid else '_none_'}")
    L.append("- Do not modify files outside the allowed paths.")
    L.append("- Never hand-edit generated files (`GENERATED FILE: DO NOT EDIT`).")
    L.append("")
    L.append("## Verify")
    for ev in evidence:
        L.append(f"- `{ev}`")
    L.append("- Run: `python3 tools/ruse.py verify --changed`")
    L.append("")
    L.append("## Done when")
    L.append("- `ruse pr check` passes (classify + artifacts + blast radius + evidence).")
    L.append("- The change author can explain every line (ai-assisted-development.md).")
    L.append("")

    lock = {
        "issue": issue,
        "roots": roots,
        "ids": included,
        "sources": source_files,
        "depth": depth,
    }
    return "\n".join(L), lock


def _cmd_build(args) -> int:
    issue = args.issue or repo.active_issue()
    roots: list[str] = []
    extra_docs: list[str] = []
    if args.ids:
        roots = args.ids
    elif args.profile:
        roots, extra_docs = _resolve_profile(args.profile)
    elif issue:
        c = contract.load(issue) or {}
        roots = contract.affected_ids(c)
    if not roots:
        render.fail("no roots: pass --ids, --profile, or an issue with affected IDs")
        return 1

    try:
        body, lock = build(issue, roots, args.depth, extra_docs)
    except ValueError as e:
        render.fail(str(e) + " — fix the IDs (see `ruse impact --from <ID>`) and retry")
        return 1

    if issue:
        wd = repo.work_dir(issue)
        os.makedirs(wd, exist_ok=True)
        cpath = os.path.join(wd, "context.md")
        lpath = os.path.join(wd, "context-lock.json")
    else:
        cpath = args.out or repo.path(".ruse", "context.md")
        lpath = os.path.splitext(cpath)[0] + "-lock.json"
        os.makedirs(os.path.dirname(cpath), exist_ok=True)
    with open(cpath, "w", encoding="utf-8") as fh:
        fh.write(body)
    with open(lpath, "w", encoding="utf-8") as fh:
        json.dump(lock, fh, indent=2)

    render.heading("context build")
    render.field("Issue", str(issue) if issue else "(ad-hoc)")
    render.field("Roots", ", ".join(lock["roots"]) or "—")
    render.field("IDs in pack", str(len(lock["ids"])))
    render.field("Sources", str(len(lock["sources"])))
    render.field("Context", repo.rel(cpath))
    render.field("Lock", repo.rel(lpath))
    render.ok("context pack built")
    return 0


def _cmd_check(args) -> int:
    issue = args.issue or repo.active_issue()
    if not issue:
        render.fail("no issue/active workspace to check")
        return 1
    lpath = os.path.join(repo.work_dir(issue), "context-lock.json")
    if not os.path.isfile(lpath):
        render.fail(f"no lock at {repo.rel(lpath)} — run `ruse context build` first")
        return 1
    lock = json.load(open(lpath))
    stale = []
    for f, old in (lock.get("sources") or {}).items():
        now = _sha(f)
        if now != old:
            stale.append((f, old, now))
    render.heading("context check")
    render.field("Issue", str(issue))
    render.field("Sources tracked", str(len(lock.get("sources") or {})))
    if not stale:
        render.ok("context pack is current")
        return 0
    render.heading("\nStale sources (changed since the pack was built)")
    for f, old, now in stale:
        state = "deleted" if now is None else "modified"
        render.bullet(f"{f}  ({state})", mark="✗")
    render.fail("context is stale — run `ruse context build` before continuing")
    return 1


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(prog="ruse context")
    sub = ap.add_subparsers(dest="sub", required=True)

    b = sub.add_parser("build", help="build a context pack")
    b.add_argument("--issue")
    b.add_argument("--profile", help="a spec/context-profiles.yaml profile name")
    b.add_argument("--ids", nargs="*", help="explicit root IDs")
    b.add_argument("--depth", type=int, default=1, help="graph hops to include (default 1)")
    b.add_argument("--out", help="output path when there is no issue")
    b.set_defaults(fn=_cmd_build)

    ch = sub.add_parser("check", help="check the pack against current sources")
    ch.add_argument("--issue")
    ch.set_defaults(fn=_cmd_check)

    args = ap.parse_args(argv)
    return args.fn(args)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
