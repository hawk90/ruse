#!/usr/bin/env python3
"""
phase_sync — reconcile GitHub Milestones from spec/phases.yaml (spec is the source; GitHub is a mirror).

PORTABLE governance capability: any repo with a spec/phases.yaml can run this to keep its GitHub Milestones
in one-way sync with the phase ladder. Idempotent — running it twice with no spec change is a no-op. The
milestone is NEVER the gate of record; membership lives in spec/phases.yaml. Milestones this tool owns are
tagged with a hidden `<!-- ruse-phase:<id> -->` marker so it can update/detect drift across title edits.

Usage:
  python3 tools/phase_sync.py            # DRY-RUN: print the reconcile plan (no mutation). exit 1 if drift.
  python3 tools/phase_sync.py --apply    # create/update milestones to match spec
  python3 tools/phase_sync.py --apply --prune   # also close ruse-owned milestones whose phase was removed

Requires the `gh` CLI, authenticated, in a GitHub repo. Read-only in dry-run.
"""
import sys, os, re, json, argparse, subprocess

try:
    import yaml
except ImportError:
    print("FAIL: PyYAML not installed"); sys.exit(1)

PHASES = "spec/phases.yaml"
MARKER = re.compile(r"<!--\s*ruse-phase:([a-z0-9-]+)\s*-->")


def _gh(args):
    """Run gh; return (rc, stdout, stderr)."""
    r = subprocess.run(["gh"] + args, capture_output=True, text=True)
    return r.returncode, r.stdout, r.stderr


def _norm(s):
    """Normalise for comparison (GitHub may rewrite CRLF / trailing space)."""
    return "\n".join(line.rstrip() for line in (s or "").replace("\r\n", "\n").strip().splitlines())


def desired(cat):
    """Return [{id, title, body}] from spec/phases.yaml, in phase order."""
    out = []
    for i, ph in enumerate(cat.get("phases") or [], 1):
        pid = ph["id"]
        feats = ", ".join(ph.get("includes") or [])
        body = (
            f"**Phase `{pid}`** · refines PRD stage `{ph.get('stage')}`\n\n"
            f"{(ph.get('goal') or '').strip()}\n\n"
            f"**Features:** {feats}\n\n"
            f"_Mirror of `spec/phases.yaml` (source of truth) — one-way synced; do not edit here._\n\n"
            f"<!-- ruse-phase:{pid} -->"
        )
        out.append({"id": pid, "title": f"{i}. {ph.get('title')}", "body": body})
    return out


def existing():
    """Return {phase-id: milestone} for milestones this tool owns (by marker)."""
    rc, out, err = _gh(["api", "--paginate", "repos/{owner}/{repo}/milestones?state=all&per_page=100"])
    if rc != 0:
        raise SystemExit(f"FAIL gh: {err.strip()[:200]}")
    owned = {}
    for m in json.loads(out or "[]"):
        mk = MARKER.search(m.get("description") or "")
        if mk:
            owned[mk.group(1)] = m
    return owned


def plan(cat):
    """Return (creates, updates, orphans) without mutating anything."""
    want, have = desired(cat), existing()
    creates, updates = [], []
    for d in want:
        cur = have.get(d["id"])
        if not cur:
            creates.append(d)
        elif _norm(cur.get("title")) != _norm(d["title"]) or _norm(cur.get("description")) != _norm(d["body"]):
            updates.append((d, cur))
    orphans = [m for pid, m in have.items() if pid not in {d["id"] for d in want} and m.get("state") == "open"]
    return creates, updates, orphans


def apply(creates, updates, orphans, prune):
    for d in creates:
        rc, out, err = _gh(["api", "repos/{owner}/{repo}/milestones",
                            "-f", f"title={d['title']}", "-f", f"description={d['body']}"])
        print(f"  created #{json.loads(out)['number']}: {d['title']}" if rc == 0 else f"  FAIL create {d['title']}: {err.strip()[:120]}")
    for d, cur in updates:
        rc, out, err = _gh(["api", "-X", "PATCH", f"repos/{{owner}}/{{repo}}/milestones/{cur['number']}",
                            "-f", f"title={d['title']}", "-f", f"description={d['body']}"])
        print(f"  updated #{cur['number']}: {d['title']}" if rc == 0 else f"  FAIL update #{cur['number']}: {err.strip()[:120]}")
    if prune:
        for m in orphans:
            rc, out, err = _gh(["api", "-X", "PATCH", f"repos/{{owner}}/{{repo}}/milestones/{m['number']}", "-f", "state=closed"])
            print(f"  closed orphan #{m['number']}: {m['title']}" if rc == 0 else f"  FAIL close #{m['number']}: {err.strip()[:120]}")


def main(argv):
    ap = argparse.ArgumentParser(prog="phase_sync", description="sync GitHub milestones from spec/phases.yaml")
    ap.add_argument("--apply", action="store_true", help="perform create/update (default is dry-run)")
    ap.add_argument("--prune", action="store_true", help="with --apply, close ruse-owned milestones dropped from spec")
    args = ap.parse_args(argv)

    if _gh(["--version"])[0] != 0:
        print("FAIL: gh CLI not found"); return 1
    cat = yaml.safe_load(open(PHASES))
    creates, updates, orphans = plan(cat)

    print("phase sync — spec/phases.yaml -> GitHub Milestones")
    for d in creates:
        print(f"  CREATE  {d['title']}")
    for d, cur in updates:
        print(f"  UPDATE  #{cur['number']} {d['title']}")
    for m in orphans:
        print(f"  ORPHAN  #{m['number']} {m['title']}  (in GitHub, not in spec{'; will close' if args.prune and args.apply else '; keep --prune to close'})")
    if not (creates or updates or orphans):
        print("  in sync — nothing to do")
        return 0

    if not args.apply:
        print(f"\ndry-run: {len(creates)} create, {len(updates)} update, {len(orphans)} orphan. Re-run with --apply.")
        return 1
    print("\napplying:")
    apply(creates, updates, orphans, args.prune)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
