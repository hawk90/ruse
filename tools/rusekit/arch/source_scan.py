"""Source scan for the module-level rules of spec/architecture.yaml (ARCH-FORBID-001, CON-TXN-ONLY-MUTATION).

The crate graph (`cargo metadata`) cannot see edges inside a crate, so these rules read the Rust source:

  - `forbidden_module_edges[*].scan` — files matching `paths` must not name anything matching a `forbid`
    pattern (an import of the forbidden module, or one of its types).
  - `txn_only_mutation` — the Document mutators (building a `Transaction`, the `*_tracked` apply/undo
    family) appear only in the files `allow` lists, so every edit goes through the transaction path that
    carries the view's marks and records the change (#691: Ex mutators that bypassed it corrupted files).

Comments and string/char literals are blanked first (prose that names a pattern is not a use), and test
code is skipped: files named `tests.rs` or `*_tests.rs`, and every `#[cfg(test)]` item. The scan is a
direct-use check, line-based after blanking; it does not follow a use through another module.
"""
from __future__ import annotations

import fnmatch
import glob
import os
import re

from rusekit import repo
from rusekit.gov.rust_discipline import strip_comments_strings

_TEST_FILE = re.compile(r"(^|/)(tests|[a-z_]+_tests)\.rs$")


def blank_test_items(code: str) -> str:
    """Blank every `#[cfg(test)]` item (a `mod … { … }` or any braced item), keeping line numbers."""
    out = list(code)
    for m in re.finditer(r"#\[cfg\(test\)\]", code):
        open_at = code.find("{", m.end())
        semi = code.find(";", m.end())
        if open_at == -1 or (semi != -1 and semi < open_at):
            end = semi + 1 if semi != -1 else m.end()  # `#[cfg(test)] mod x;` or a `use`
        else:
            depth, j = 0, open_at
            while j < len(code):
                if code[j] == "{":
                    depth += 1
                elif code[j] == "}":
                    depth -= 1
                    if depth == 0:
                        break
                j += 1
            end = j + 1
        for k in range(m.start(), min(end, len(code))):
            if out[k] != "\n":
                out[k] = " "
    return "".join(out)


def _files(patterns: list[str]) -> list[str]:
    found: set[str] = set()
    for p in patterns:
        for path in glob.glob(os.path.join(repo.ROOT, p), recursive=True):
            if os.path.isfile(path):
                found.add(os.path.relpath(path, repo.ROOT).replace(os.sep, "/"))
    return sorted(f for f in found if not _TEST_FILE.search(f))


def _code(rel: str) -> str:
    src = open(os.path.join(repo.ROOT, rel), encoding="utf-8").read()
    return blank_test_items(strip_comments_strings(src))


def scan(paths: list[str], forbid: list[str], allow: list[str] | None = None) -> list[tuple[str, int, str]]:
    """`(file, line, pattern)` for every use of a `forbid` pattern in non-test code of `paths`, outside
    the files `allow` matches."""
    pats = [re.compile(p) for p in forbid]
    hits: list[tuple[str, int, str]] = []
    for rel in _files(paths):
        if any(fnmatch.fnmatch(rel, a) for a in (allow or [])):
            continue
        for ln, line in enumerate(_code(rel).splitlines(), 1):
            for p in pats:
                if p.search(line):
                    hits.append((rel, ln, p.pattern))
    return hits


def check(arch: dict) -> tuple[list[str], list[str]]:
    """Run every source rule `arch` (spec/architecture.yaml) declares. Returns `(errors, notes)`; a rule
    with no `scan` block is reported as not source-checked rather than passing silently."""
    errors: list[str] = []
    notes: list[str] = []
    for e in arch.get("forbidden_module_edges") or []:
        name = f"{e.get('from')} → {e.get('to')}"
        sc = e.get("scan")
        if not sc:
            notes.append(f"forbidden edge {name}: no scan declared (not source-checked)")
            continue
        hits = scan(sc.get("paths") or [], sc.get("forbid") or [], sc.get("allow"))
        for rel, ln, pat in hits:
            errors.append(f"forbidden edge {name}: {rel}:{ln} matches `{pat}` ({e.get('reason', '')})")
        notes.append(f"forbidden edge {name}: {len(_files(sc.get('paths') or []))} files scanned")
    t = arch.get("txn_only_mutation")
    if t:
        hits = scan(t.get("paths") or [], t.get("forbid") or [], t.get("allow"))
        for rel, ln, pat in hits:
            errors.append(f"direct document mutation: {rel}:{ln} matches `{pat}` — edits go through "
                          f"{', '.join(t.get('allow') or [])} (CON-TXN-ONLY-MUTATION, #691)")
        notes.append(f"txn-only mutation: {len(_files(t.get('paths') or []))} files scanned")
    return errors, notes
