"""Size scan for `gov ratchet` (RA-STRUCT-005): non-test functions over 150 lines, files over 1,000 lines.

The rule and its targets (100 lines for a function, 800 for a file) are in
docs/contributing/size-ceilings.md; this module only measures. The scan reads the non-test `.rs` files under
`crates/` and `apps/`:

  - a test path is skipped: any file under a `tests/` or `benches/` directory, and files named `tests.rs`,
    `test.rs` or `*_tests.rs`;
  - comments, strings (plain, byte, raw) and char literals are blanked first, so a brace inside one does
    not move a function's end;
  - the lines of a `#[cfg(test)]` module do not count toward its file, and its functions are not measured;
  - a function runs from its `fn` line to the line of its closing brace (a body-less `fn …;` is skipped).
    A function inside a top-level `impl` is named `Type::name`.

Data tables are exempt by name (`EXEMPT_FILES`, `EXEMPT_FNS`): their length is the data, not logic to split.
"""
from __future__ import annotations

import os
import re

FN_LIMIT = 150     # a function longer than this counts in `fn_over_150`
FILE_LIMIT = 1000  # a file with more non-test lines than this counts in `file_over_1000`

EXEMPT_FILES = {
    "crates/core/src/case_tables.rs": "generated Unicode case tables",
    "apps/tui/src/input/set_cmd/table.rs": "the :set option table (one row per option)",
    "crates/core/src/editor/ex_cmd/samples.rs": "the Ex command sample table",
}
EXEMPT_FNS = {
    ("apps/tui/src/input/emacs.rs", "EmacsProfile::new"): "the Emacs key-binding table",
}

_RAW = re.compile(r'b?r(#*)"')
_CHAR = re.compile(r"'(\\(x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]+\}|.)|[^\\'\n])'")
_FN = re.compile(r'^\s*(pub(\([^)]*\))?\s+)?((const|async|unsafe|extern(\s+"[^"]*")?)\s+)*'
                 r'fn\s+([A-Za-z_][A-Za-z0-9_]*)')
_IMPL = re.compile(r'^impl\b(?:\s*<[^{]*?>)?\s+(?:[^{]*?\s+for\s+)?([A-Za-z_][A-Za-z0-9_]*)')
_CFG_TEST = re.compile(r"#\[cfg\(test\)\]")
_MOD = re.compile(r"^\s*(pub(\([^)]*\))?\s+)?mod\s+\w+\s*\{")


def is_test_path(rel: str) -> bool:
    parts = rel.replace(os.sep, "/").split("/")
    name = parts[-1]
    return ("tests" in parts[:-1] or "benches" in parts[:-1]
            or name in ("tests.rs", "test.rs") or name.endswith("_tests.rs"))


def _blank(text: str) -> str:
    return re.sub(r"[^\n]", " ", text)


def _ident_before(src: str, i: int) -> bool:
    return i > 0 and (src[i - 1].isalnum() or src[i - 1] == "_")


def mask(src: str) -> str:
    """Blank comments, strings and char literals, keeping every newline (so line numbers hold)."""
    out: list[str] = []
    i, n = 0, len(src)
    while i < n:
        c = src[i]
        if src.startswith("//", i):
            j = src.find("\n", i)
            j = n if j < 0 else j
            out.append(" " * (j - i))
            i = j
        elif src.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if src.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif src.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            out.append(_blank(src[i:j]))
            i = j
        elif (m := _RAW.match(src, i)) and not _ident_before(src, i):
            close = '"' + m.group(1)
            end = src.find(close, m.end())
            end = n if end < 0 else end + len(close)
            out.append(_blank(src[i:end]))
            i = end
        elif c == '"' or (src.startswith('b"', i) and not _ident_before(src, i)):
            j = i + (2 if c == "b" else 1)
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            j += 1
            out.append(_blank(src[i:j]))
            i = j
        elif c == "'" and (m := _CHAR.match(src, i)):
            out.append(" " * (m.end() - i))
            i = m.end()
        else:
            out.append(c)
            i += 1
    return "".join(out)


def _block_end(lines: list[str], start: int, col: int = 0) -> int | None:
    """The line index of the brace closing the first `{` at or after (start, col), or None when a `;`
    comes before any `{` (a declaration with no body)."""
    depth, opened = 0, False
    for e in range(start, len(lines)):
        for ch in lines[e][col if e == start else 0:]:
            if ch == "{":
                depth, opened = depth + 1, True
            elif ch == "}":
                depth -= 1
            elif ch == ";" and not opened:
                return None
            if opened and depth == 0:
                return e
    return len(lines) - 1 if opened else None


def _test_lines(lines: list[str]) -> set[int]:
    """Line indexes of every `#[cfg(test)]` module, its attribute lines included."""
    skip: set[int] = set()
    k = 0
    while k < len(lines):
        if _CFG_TEST.search(lines[k]):
            j = k + 1
            while j < len(lines) and lines[j].strip().startswith("#["):
                j += 1
            if j < len(lines) and _MOD.match(lines[j]):
                end = _block_end(lines, j)
                end = j if end is None else end
                skip.update(range(k, end + 1))
                k = end + 1
                continue
        k += 1
    return skip


def _impl_names(lines: list[str]) -> dict[int, str]:
    """Line index -> the type of the top-level `impl` block that line is inside."""
    owner: dict[int, str] = {}
    k = 0
    while k < len(lines):
        m = _IMPL.match(lines[k])
        end = _block_end(lines, k) if m else None
        if m and end is not None:
            for ln in range(k, end + 1):
                owner[ln] = m.group(1)
            k = end + 1
        else:
            k += 1
    return owner


def scan_source(src: str) -> tuple[int, list[tuple[int, str, int]]]:
    """`(non-test line count, [(1-based line, name, length)] for every non-test fn)` of one file."""
    lines = mask(src).split("\n")
    total = len(lines) - (1 if src.endswith("\n") else 0)
    skip = _test_lines(lines)
    owner = _impl_names(lines)
    fns: list[tuple[int, str, int]] = []
    for idx, line in enumerate(lines):
        if idx in skip or not (m := _FN.match(line)):
            continue
        end = _block_end(lines, idx, m.end())
        if end is None:
            continue
        name = f"{owner[idx]}::{m.group(6)}" if idx in owner else m.group(6)
        fns.append((idx + 1, name, end - idx + 1))
    return total - len(skip), fns


def source_files(root: str) -> list[str]:
    """Repo-relative `/` paths of the non-test `.rs` files under crates/ and apps/."""
    out: list[str] = []
    for top in ("crates", "apps"):
        for dp, dn, names in os.walk(os.path.join(root, top)):
            dn[:] = sorted(d for d in dn if d not in ("target", "tests", "benches"))
            for f in sorted(names):
                rel = os.path.relpath(os.path.join(dp, f), root).replace(os.sep, "/")
                if f.endswith(".rs") and not is_test_path(rel):
                    out.append(rel)
    return out


def scan(root: str) -> dict:
    """`{"fn_over_150": [(path, line, name, length)], "file_over_1000": [(path, 1, "", lines)]}`, each
    sorted longest first, the exempt tables left out."""
    long_fns, big_files = [], []
    for rel in source_files(root):
        if rel in EXEMPT_FILES:
            continue
        with open(os.path.join(root, rel), encoding="utf-8") as fh:
            lines, fns = scan_source(fh.read())
        if lines > FILE_LIMIT:
            big_files.append((rel, 1, "", lines))
        long_fns.extend((rel, ln, name, length) for ln, name, length in fns
                        if length > FN_LIMIT and (rel, name) not in EXEMPT_FNS)
    key = (lambda r: (-r[3], r[0], r[1]))
    return {"fn_over_150": sorted(long_fns, key=key), "file_over_1000": sorted(big_files, key=key)}


def describe(row: tuple[str, int, str, int]) -> str:
    rel, ln, name, size = row
    return f"{rel}:{ln} {name} {size}" if name else f"{rel} {size}"
