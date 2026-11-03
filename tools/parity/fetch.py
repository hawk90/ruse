#!/usr/bin/env python3
"""parity fetch — materialise each SHA-pinned upstream into the local cache.

Clones are NEVER vendored into the repo. ruse is MIT; Vim ships under the Vim license and Emacs
under GPL-3.0, so upstream files stay in ${XDG_CACHE_HOME:-~/.cache}/ruse-dev/parity/<editor>/ (outside
the repo, shared across worktrees, D-062) and only
extracted FACTS (names, types, defaults, scopes) reach spec/parity/. Facts about how a program
behaves are not the program's expression; its files are.

Fetching is by commit SHA, not tag or branch: a census is only meaningful against an exact
revision (spec/parity/upstreams.yaml rule 1). If the pin cannot be fetched the command fails
rather than silently surveying whatever HEAD happens to be.

  python3 tools/parity/fetch.py              # every upstream in upstreams.yaml
  python3 tools/parity/fetch.py neovim       # one
  python3 tools/parity/fetch.py --check      # report cache state, fetch nothing
"""
from __future__ import annotations

import argparse
import os
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import yaml  # noqa: E402

from rusekit import render, repo  # noqa: E402

UPSTREAMS = "spec/parity/upstreams.yaml"
CACHE = "parity"   # under repo.cache_root()


def load_upstreams() -> dict:
    with open(repo.path(UPSTREAMS), encoding="utf-8") as fh:
        return yaml.safe_load(fh) or {}


def cache_dir(editor: str) -> str:
    return os.path.join(repo.cache_root(), CACHE, editor)


def _run(args: list[str], cwd: str) -> tuple[int, str]:
    p = subprocess.run(args, cwd=cwd, capture_output=True, text=True)
    return p.returncode, (p.stdout or "") + (p.stderr or "")


def head_sha(path: str) -> str | None:
    if not os.path.isdir(os.path.join(path, ".git")):
        return None
    code, out = _run(["git", "rev-parse", "HEAD"], path)
    return out.strip() if code == 0 else None


def fetch_one(editor: str, spec: dict) -> tuple[bool, str]:
    """Fetch exactly `spec['revision']` into the cache. Idempotent: an already-correct cache is a no-op.

    The cache is shared by every worktree, so the fetch holds an exclusive lock per upstream: two
    concurrent `parity fetch` runs serialise, and the second finds the pin already checked out."""
    with repo.cache_lock(os.path.dirname(cache_dir(editor)), editor):
        return _fetch_locked(editor, spec)


def _fetch_locked(editor: str, spec: dict) -> tuple[bool, str]:
    want = str(spec.get("revision") or "")
    if len(want) != 40 or not all(ch in "0123456789abcdef" for ch in want):
        return False, f"revision must be a full 40-char commit SHA (got {want!r}) — tags and ranges are not pins"

    path = cache_dir(editor)
    have = head_sha(path)
    if have == want:
        return True, f"cached at {want[:12]} ({spec.get('version_label', '?')})"

    if not os.path.isdir(os.path.join(path, ".git")):
        # Initialise under a temp name and rename into place, so an interrupted init never leaves a
        # half-made clone that a later run would mistake for the cache.
        tmp = f"{path}.tmp-{os.getpid()}"
        os.makedirs(tmp, exist_ok=True)
        code, out = _run(["git", "init", "-q"], tmp)
        if code != 0:
            return False, f"git init failed: {out.strip()[:200]}"
        _run(["git", "remote", "add", "origin", spec["repo"]], tmp)
        if os.path.isdir(path) and not os.listdir(path):
            os.rmdir(path)
        os.rename(tmp, path)

    # Depth-1 fetch of the exact commit. Works on GitHub for tag/branch-reachable SHAs; a pin that
    # has been GC'd or force-pushed away must fail loudly, not fall back to HEAD.
    code, out = _run(["git", "fetch", "--depth", "1", "--quiet", "origin", want], path)
    if code != 0:
        return False, f"could not fetch pinned SHA {want[:12]}: {out.strip()[:200]}"
    code, out = _run(["git", "checkout", "-q", "--detach", "FETCH_HEAD"], path)
    if code != 0:
        return False, f"checkout failed: {out.strip()[:200]}"

    got = head_sha(path)
    if got != want:
        return False, f"checked out {got} but pin is {want}"
    return True, f"fetched {want[:12]} ({spec.get('version_label', '?')})"


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(prog="parity fetch")
    ap.add_argument("editors", nargs="*", help="which upstreams (default: all)")
    ap.add_argument("--check", action="store_true", help="report cache state without fetching")
    args = ap.parse_args(argv if argv is not None else sys.argv[1:])

    doc = load_upstreams()
    ups = doc.get("upstreams") or {}
    want = args.editors or sorted(ups)

    render.heading("parity fetch (SHA-pinned upstream cache)")
    render.field("Cache", os.path.join(repo.cache_root(), CACHE)
                 + "/  (outside the repo — upstream files are never vendored)")

    rc = 0
    for editor in want:
        spec = ups.get(editor)
        if not spec:
            render.bullet(f"{editor}: not declared in {UPSTREAMS}", mark="!")
            rc = 1
            continue
        if args.check:
            have = head_sha(cache_dir(editor))
            pin = str(spec.get("revision", ""))[:12]
            synced = have == spec.get("revision")
            state = "in sync" if synced else f"STALE (cache {have[:12] if have else 'absent'})"
            render.bullet(f"{editor}: pin {pin} — {state}", mark="-" if synced else "!")
            rc |= 0 if synced else 1
            continue
        okd, msg = fetch_one(editor, spec)
        render.bullet(f"{editor}: {msg}", mark="-" if okd else "!")
        rc |= 0 if okd else 1

    if rc:
        render.fail("cache is not at the pinned revision — run `python3 tools/parity/fetch.py`"
                    if args.check else
                    "one or more upstreams could not be materialised at their pinned revision")
    else:
        render.ok(f"{len(want)} upstream(s) at their pinned revision")
    return rc


if __name__ == "__main__":
    raise SystemExit(main())
