#!/usr/bin/env python3
"""Generate ruse's case tables from the pinned Neovim and Emacs (#912, #1112, #732).

ruse's Vim profile recases and case-folds the way Neovim does, and its Emacs profile recases the way
Emacs does. Neither editor uses Rust's `char::to_uppercase` / the `regex` crate's fold table, and the
three disagree on hundreds of characters (ß, ǅ, ı, ſ, the Greek iota-subscript letters, letters newer
than a library's Unicode version, ...). So the tables are CAPTURED from the pinned binaries, never
hand-written, and the generated Rust file records the versions they came from.

What is captured, and from where:

  Neovim (v0.12.6, `nvim --headless -u NONE -i NONE -l`):
    * `toupper()` / `tolower()` of every code point: nvim's `mb_toupper` / `mb_tolower` (default
      'casemap' "internal,keepascii": ASCII by A-Z/a-z, the rest by the utf8proc nvim links). These
      are what `gU` `gu` `g~` `~` use (`swapchar`).
    * `utf_fold` equivalence, which 'ignorecase' matching uses (`utf_fold(a) == utf_fold(b)` in the
      regexp engines). nvim has no function that returns `utf_fold`, so the classes are built from
      equality tests: `a ==? b` compares by `utf_strnicmp`, which is `utf_fold(a) - utf_fold(b)` for
      two single characters. The candidate pairs are every character with its nvim upper and lower
      case and with its single-character Unicode full case fold (Python's `str.casefold`, which is
      what utf8proc's UTF8PROC_CASEFOLD implements); nvim decides which of them are equal.
  Emacs (31.1, `emacs -Q --batch`):
    * `(upcase S)` / `(downcase S)` / `(capitalize S)` of each one-character string S: casefiddle.c's
      `case_character` with the unconditional special casing (ß -> SS, ﬁ -> FI, İ -> i̇, ...) and the
      standard case table (which leaves ı and ſ alone). On a one-character string the final-sigma rule
      never fires; ruse applies that rule itself, as casefiddle.c does.
    * which characters have word syntax in the standard syntax table (casefiddle.c's `inword`).

Usage:
    python3 tools/parity/case_tables.py            # regenerate crates/core/src/case_tables.rs
    python3 tools/parity/case_tables.py --check    # fail if the committed file is stale
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unicodedata
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
OUT = REPO_ROOT / "crates" / "core" / "src" / "case_tables.rs"
sys.path.insert(0, str(Path(__file__).resolve().parent))

import emacs_oracle  # noqa: E402
import oracle  # noqa: E402

MAX_CP = 0x10FFFF


def _is_scalar(c: int) -> bool:
    return not (0xD800 <= c <= 0xDFFF)


# Lua: every code point's toupper/tolower (only the ones that change), then `==?` over candidate pairs.
_NVIM_LUA = r"""
local pairs_in = vim.json.decode(io.open(%q):read('*a'))
local function enc(c)
  if c < 0x80 then return string.char(c)
  elseif c < 0x800 then return string.char(0xC0 + bit.rshift(c, 6), 0x80 + bit.band(c, 0x3F))
  elseif c < 0x10000 then
    return string.char(0xE0 + bit.rshift(c, 12), 0x80 + bit.band(bit.rshift(c, 6), 0x3F), 0x80 + bit.band(c, 0x3F))
  end
  return string.char(0xF0 + bit.rshift(c, 18), 0x80 + bit.band(bit.rshift(c, 12), 0x3F),
    0x80 + bit.band(bit.rshift(c, 6), 0x3F), 0x80 + bit.band(c, 0x3F))
end
local up, low = {}, {}
local toupper, tolower, char2nr = vim.fn.toupper, vim.fn.tolower, vim.fn.char2nr
for c = 1, 0x10FFFF do
  if c < 0xD800 or c > 0xDFFF then
    local s = enc(c)
    local u = toupper(s)
    if u ~= s then up[#up + 1] = { c, char2nr(u) } end
    local l = tolower(s)
    if l ~= s then low[#low + 1] = { c, char2nr(l) } end
  end
end
vim.cmd('function! RuseEqIc(a, b)\n return a:a ==? a:b\nendfunction')
local eq = {}
for i, p in ipairs(pairs_in) do
  eq[i] = vim.fn.RuseEqIc(enc(p[1]), enc(p[2])) == 1
end
io.write(vim.json.encode({ up = up, low = low, eq = eq }))
"""

_EMACS_EL = r"""
(let ((up nil) (down nil) (title nil) (word nil) (run-start nil))
  (dotimes (c (1+ #x10FFFF))
    (when (and (characterp c) (not (<= #xD800 c #xDFFF)))
      (let ((s (string c)))
        ;; Results go out as code-point vectors, so the JSON is ASCII whatever batch's output coding.
        (let ((u (upcase s))) (unless (equal u s) (push (list c (vconcat u)) up)))
        (let ((d (downcase s))) (unless (equal d s) (push (list c (vconcat d)) down)))
        (let ((k (capitalize s))) (unless (equal k s) (push (list c (vconcat k)) title))))
      (let ((w (eq (char-syntax c) ?w)))
        (cond ((and w (not run-start)) (setq run-start c))
              ((and (not w) run-start) (push (list run-start (1- c)) word) (setq run-start nil))))))
  (when run-start (push (list run-start #x10FFFF) word))
  (let ((conv (lambda (l) (vconcat (mapcar (lambda (e) (vconcat e)) (nreverse l))))))
    (princ (json-serialize (list :up (funcall conv up) :down (funcall conv down)
                                 :title (funcall conv title) :word (funcall conv word))))))
"""


def run_nvim(candidates: list[tuple[int, int]]) -> dict:
    oracle.assert_pin(oracle.nvim_version(), oracle.read_pin())
    with tempfile.TemporaryDirectory() as tmp:
        pairs = Path(tmp) / "pairs.json"
        pairs.write_text(json.dumps(candidates), encoding="utf-8")
        script = Path(tmp) / "probe.lua"
        script.write_text(_NVIM_LUA.replace("%q", json.dumps(str(pairs))), encoding="utf-8")
        proc = subprocess.run(
            ["nvim", "--headless", "-u", "NONE", "-i", "NONE", "-l", str(script)],
            capture_output=True,
            text=True,
            timeout=600,
        )
    if proc.returncode != 0 or not proc.stdout:
        raise SystemExit(f"nvim probe failed (rc={proc.returncode}): {proc.stderr.strip()}")
    return json.loads(proc.stdout)


def run_emacs() -> dict:
    pin = emacs_oracle.read_pin()
    version = emacs_oracle.emacs_version()
    emacs_oracle.assert_pin(version, pin)
    with tempfile.TemporaryDirectory() as tmp:
        script = Path(tmp) / "probe.el"
        script.write_text(";; -*- lexical-binding: t -*-\n" + _EMACS_EL, encoding="utf-8")
        proc = subprocess.run(
            ["emacs", "-Q", "--batch", "-l", str(script)],
            capture_output=True,
            timeout=600,
        )
    if proc.returncode != 0 or not proc.stdout:
        raise SystemExit(f"emacs probe failed (rc={proc.returncode}): {proc.stderr.decode(errors='replace')}")
    return json.loads(proc.stdout.decode("utf-8"))


def fold_classes(up: dict[int, int], low: dict[int, int], eq_of) -> list[list[int]]:
    """Union the candidate pairs nvim reports `==?`-equal into `utf_fold` classes of two or more."""
    cand: set[tuple[int, int]] = set()
    for c in range(MAX_CP + 1):
        if not _is_scalar(c):
            continue
        f = chr(c).casefold()
        if len(f) == 1 and ord(f) != c:
            cand.add((min(c, ord(f)), max(c, ord(f))))
    for m in (up, low):
        for a, b in m.items():
            cand.add((min(a, b), max(a, b)))
    pairs = sorted(cand)
    eq = eq_of(pairs)
    parent: dict[int, int] = {}

    def find(x: int) -> int:
        while parent.get(x, x) != x:
            parent[x] = parent.get(parent[x], parent[x])
            x = parent[x]
        return x

    for (a, b), same in zip(pairs, eq):
        if same:
            ra, rb = find(a), find(b)
            if ra != rb:
                parent[max(ra, rb)] = min(ra, rb)
    groups: dict[int, list[int]] = {}
    for x in {x for p in pairs for x in p}:
        groups.setdefault(find(x), []).append(x)
    return sorted(sorted(g) for g in groups.values() if len(g) > 1)


def rust_str(s: str) -> str:
    return '"' + "".join(f"\\u{{{ord(ch):X}}}" for ch in s) + '"'


def render(nv: dict, classes: list[list[int]], em: dict, nvim_ver: str, emacs_ver: str) -> str:
    out: list[str] = []
    w = out.append
    w("// @generated by tools/parity/case_tables.py. Do not edit; rerun the script instead.")
    w(f"// Neovim: {nvim_ver} (toupper/tolower, `==?` for utf_fold); candidate folds from Python's")
    w(f"// Unicode {unicodedata.unidata_version} str.casefold. Emacs: {emacs_ver} (upcase/downcase/capitalize")
    w("// of one-character strings, standard syntax table word characters).")
    w("")
    w(f'pub(crate) const NVIM_VERSION: &str = "{nvim_ver}";')
    w(f'pub(crate) const EMACS_VERSION: &str = "{emacs_ver}";')
    w("")
    for name, rows in (("NVIM_TOUPPER", nv["up"]), ("NVIM_TOLOWER", nv["low"])):
        w(f"/// nvim `{name[5:].lower()}()` of every character it changes, sorted by character.")
        w(f"pub(crate) const {name}: &[(u32, u32)] = &[")
        for a, b in rows:
            w(f"    (0x{a:X}, 0x{b:X}),")
        w("];")
        w("")
    w("/// nvim `utf_fold` classes: characters 'ignorecase' matches with each other, two or more each.")
    w("pub(crate) const NVIM_FOLD_CLASSES: &[&[u32]] = &[")
    for g in classes:
        w("    &[" + ", ".join(f"0x{x:X}" for x in g) + "],")
    w("];")
    w("")
    index = sorted((x, i) for i, g in enumerate(classes) for x in g)
    w("/// Each character of [`NVIM_FOLD_CLASSES`] and the index of its class, sorted by character.")
    w("pub(crate) const NVIM_FOLD_INDEX: &[(u32, u16)] = &[")
    for x, i in index:
        w(f"    (0x{x:X}, {i}),")
    w("];")
    w("")
    for key, name in (("up", "EMACS_UPCASE"), ("down", "EMACS_DOWNCASE"), ("title", "EMACS_CAPITALIZE")):
        w(f"/// Emacs `({key if key != 'title' else 'capitalize'} S)` of every one-character string it changes.")
        w(f"pub(crate) const {name}: &[(u32, &str)] = &[")
        for c, cps in em[key]:
            w(f"    (0x{c:X}, {rust_str(''.join(map(chr, cps)))}),")
        w("];")
        w("")
    w("/// Ranges of characters with word syntax in Emacs's standard syntax table.")
    w("pub(crate) const EMACS_WORD: &[(u32, u32)] = &[")
    for a, b in em["word"]:
        w(f"    (0x{a:X}, 0x{b:X}),")
    w("];")
    return "\n".join(out) + "\n"


def generate() -> str:
    nvim_ver = oracle.nvim_version()
    probe: dict = {}

    def eq_of(pairs: list[tuple[int, int]]) -> list[bool]:
        probe.update(run_nvim([list(p) for p in pairs]))
        return probe["eq"]

    # The upper/lower maps feed the candidates, so take them from a first run, then ask for equality.
    first = run_nvim([])
    up = {a: b for a, b in first["up"]}
    low = {a: b for a, b in first["low"]}
    classes = fold_classes(up, low, eq_of)
    for c in classes:
        assert len(c) < 64, c
    em = run_emacs()
    emacs_ver = emacs_oracle.emacs_version()
    return render(first, classes, em, nvim_ver, emacs_ver)


def main(argv: list[str]) -> int:
    text = generate()
    if "--check" in argv:
        if OUT.read_text(encoding="utf-8") != text:
            print(f"{OUT.relative_to(REPO_ROOT)} is stale: rerun tools/parity/case_tables.py", file=sys.stderr)
            return 1
        print("case tables are current")
        return 0
    OUT.write_text(text, encoding="utf-8")
    print(f"wrote {OUT.relative_to(REPO_ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
