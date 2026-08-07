#!/usr/bin/env python3
"""gov rust_discipline — the two stability-discipline rules clippy structurally CANNOT express (D-041).

The stability doc's "v0 decision table" is enforced by ONE mechanism per rule — the most accurate one. Every
AST-expressible rule belongs to clippy (the required `rust` check), configured once and not reimplemented
here (avoiding the harness duplication that a fragile Python re-scan would be):

  - `print_stdout` / `print_stderr` — crate-root `#![deny(...)]` (logs go through `tracing`).
  - a non-test `.unwrap()`         — crate-root `#![deny(clippy::unwrap_used)]` + `allow-unwrap-in-tests`
                                     in clippy.toml (native, AST-accurate, test-exemption for free).
  - `catch_unwind`                 — `disallowed-methods` in clippy.toml (STAB-6).

This checker owns ONLY what clippy cannot see, so there is no overlap:

  1. `Result<_, String>`         — a stringly-typed error the design rejects (§2). clippy has no lint for a
                                   specific generic argument, so a source scan is the only option.
  2. `panic = "abort"` (Cargo)   — kills crash-recovery (STAB-5). clippy lints Rust, not Cargo manifests.

Comments and string/char literals are blanked before scanning so prose that merely NAMES a pattern (this
repo's own design docs and `recover.rs`'s module doc) is never flagged. Blocking by default (a gate); the
tree satisfied both rules at adoption, so there is no annotation debt — it only catches future drift.
"""
from __future__ import annotations

import argparse
import glob
import os
import re
import sys

from rusekit import render

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))
SRC_GLOBS = ["crates/*/src/**/*.rs", "apps/*/src/**/*.rs"]


def strip_comments_strings(src: str) -> str:
    """Blank // and /* */ comments and string/char literals (raw strings incl.), preserving line numbers."""
    out = list(src)
    i, n = 0, len(src)

    def blank(a: int, b: int) -> None:
        for k in range(a, min(b, n)):
            if out[k] != "\n":
                out[k] = " "

    while i < n:
        two = src[i : i + 2]
        if two == "//":
            j = src.find("\n", i)
            j = n if j == -1 else j
            blank(i, j)
            i = j
        elif two == "/*":
            depth, j = 1, i + 2
            while j < n and depth:
                if src[j : j + 2] == "/*":
                    depth += 1
                    j += 2
                elif src[j : j + 2] == "*/":
                    depth -= 1
                    j += 2
                else:
                    j += 1
            blank(i, j)
            i = j
        elif src[i] == "r" and i + 1 < n and src[i + 1] in '"#':
            h, j = 0, i + 1
            while j < n and src[j] == "#":
                h, j = h + 1, j + 1
            if j < n and src[j] == '"':
                close = '"' + "#" * h
                end = src.find(close, j + 1)
                end = n if end == -1 else end + len(close)
                blank(i, end)
                i = end
            else:
                i += 1
        elif src[i] == '"':
            j = i + 1
            while j < n:
                if src[j] == "\\":
                    j += 2
                    continue
                if src[j] == '"':
                    j += 1
                    break
                j += 1
            blank(i, j)
            i = j
        elif src[i] == "'":
            m = re.match(r"'(\\.|[^'\\])'", src[i : i + 4])
            if m:
                blank(i, i + m.end())
                i += m.end()
            else:
                i += 1
        else:
            i += 1
    return "".join(out)


def scan_source() -> list[dict]:
    findings: list[dict] = []
    files: list[str] = []
    for g in SRC_GLOBS:
        files.extend(glob.glob(os.path.join(ROOT, g), recursive=True))
    for path in sorted(set(files)):
        rel = os.path.relpath(path, ROOT)
        code = strip_comments_strings(open(path, encoding="utf-8").read())
        for ln, line in enumerate(code.splitlines(), 1):
            if re.search(r"Result\s*<[^>]*,\s*String\s*>", line):
                findings.append({"rel": rel, "ln": ln, "rule": "string-error",
                                 "msg": "`Result<_, String>` — use a typed error enum (stability §2)"})
    return findings


def scan_manifests() -> list[dict]:
    findings: list[dict] = []
    for path in sorted(glob.glob(os.path.join(ROOT, "**", "Cargo.toml"), recursive=True)):
        if os.sep + "target" + os.sep in path:
            continue
        rel = os.path.relpath(path, ROOT)
        for ln, line in enumerate(open(path, encoding="utf-8").read().splitlines(), 1):
            if re.search(r'panic\s*=\s*"abort"', line.split("#", 1)[0]):
                findings.append({"rel": rel, "ln": ln, "rule": "panic-abort",
                                 "msg": '`panic = "abort"` kills crash-recovery (STAB-5)'})
    return findings


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(prog="ruse gov rust_discipline")
    ap.add_argument("--warn", action="store_true", help="report but do not fail (default: block)")
    args = ap.parse_args(argv)

    findings = scan_source() + scan_manifests()

    render.heading("gov rust_discipline (D-041: the two rules clippy can't express)")
    render.field("Banned patterns found", str(len(findings)))
    for f in findings:
        render.bullet(f"{f['rel']}:{f['ln']}  [{f['rule']}] {f['msg']}", mark="!")

    if not findings:
        render.ok("no stringly-typed `Result<_, String>` and no `panic = \"abort\"` (D-041); "
                  "clippy owns unwrap/catch_unwind/print")
        return 0
    msg = f"{len(findings)} discipline violation(s) — see the stability v0 decision table (D-041)"
    if args.warn:
        render.warn(msg + " (warn)")
        return 0
    render.fail(msg)
    return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
