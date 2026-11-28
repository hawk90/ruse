#!/usr/bin/env python3
"""gov proptest_seeds — every committed proptest regression seed file must map to an existing source file.

proptest replays a seed file only for the test source file that maps onto it, so a seed file whose source was
moved or deleted never runs again, and nothing reports it (#1585: apps/tui/proptest-regressions/input.txt
outlived src/input.rs). The mapping is proptest's default `FileFailurePersistence::SourceParallel`:

  R1  `<D>/proptest-regressions/<P>.txt` maps to `<D>/<C>/<P>.rs`, where `<C>` is a child directory of `<D>`
      that holds a `lib.rs` or `main.rs` (normally `src`, so `src/foo/bar.rs` -> `proptest-regressions/foo/bar.txt`).
  R2  a source with no `lib.rs`/`main.rs` above it (an integration test under `tests/`) falls back to
      `WithSource`: `<dir>/<name>.rs` -> `<dir>/<name>.proptest-regressions`, next to the source.

FAILS on any seed file whose source does not exist. Auto-discovered into `ruse gov check`.
"""
from __future__ import annotations

import argparse
import os
import sys

from rusekit import render, repo  # noqa: E402

SIBLING = "proptest-regressions"
SUFFIX = ".proptest-regressions"
PRUNE = {".git", "target", ".claude", ".ruse", "node_modules"}


def seed_files(root: str) -> list[str]:
    """Repo-relative paths (`/`-separated) of every proptest seed file under root."""
    found: list[str] = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d not in PRUNE)
        in_sibling = SIBLING in os.path.relpath(dirpath, root).split(os.sep)
        for name in sorted(filenames):
            if (in_sibling and name.endswith(".txt")) or name.endswith(SUFFIX):
                found.append(os.path.relpath(os.path.join(dirpath, name), root).replace(os.sep, "/"))
    return found


def source_candidates(root: str, seed: str) -> list[str]:
    """Repo-relative source paths that proptest would map onto `seed`; empty if the name fits neither rule."""
    if seed.endswith(SUFFIX):
        return [seed[: -len(SUFFIX)] + ".rs"]
    parts = seed.split("/")
    if SIBLING not in parts:
        return []
    i = len(parts) - 1 - parts[::-1].index(SIBLING)
    base, tail = parts[:i], parts[i + 1:]
    if not tail or not tail[-1].endswith(".txt"):
        return []
    tail = tail[:-1] + [tail[-1][: -len(".txt")] + ".rs"]
    base_abs = os.path.join(root, *base)
    out: list[str] = []
    for child in sorted(os.listdir(base_abs)) if os.path.isdir(base_abs) else []:
        cdir = os.path.join(base_abs, child)
        if child != SIBLING and os.path.isdir(cdir) and (
                os.path.isfile(os.path.join(cdir, "lib.rs")) or os.path.isfile(os.path.join(cdir, "main.rs"))):
            out.append("/".join(base + [child] + tail))
    return out


def orphans(root: str) -> list[tuple[str, list[str]]]:
    """(seed, candidates) for each seed file none of whose candidate sources exists."""
    bad = []
    for seed in seed_files(root):
        cands = source_candidates(root, seed)
        if not any(os.path.isfile(os.path.join(root, *c.split("/"))) for c in cands):
            bad.append((seed, cands))
    return bad


def main(argv=None) -> int:
    argparse.ArgumentParser(prog="gov proptest_seeds").parse_args(argv or [])
    root = repo.path()
    seeds = seed_files(root)
    bad = orphans(root)

    render.heading("gov proptest_seeds (every proptest seed file maps to an existing source)")
    render.field("Seed files", str(len(seeds)))
    for seed, cands in bad:
        want = " or ".join(cands) if cands else "no source (path fits no proptest mapping)"
        render.bullet(f"{seed}: orphaned, expected {want}", mark="!")

    if bad:
        render.fail(f"{len(bad)} orphaned proptest seed file(s): move the seeds to the test's new file or drop them")
        return 1
    render.ok("every proptest seed file maps to an existing source")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
