#!/usr/bin/env python3
"""Classify the Vim parity corpus by scenario class and report thin classes (#901).

The corpus (`tests/parity/vim/fixtures/corpus.yaml`) is a flat list of (lines, keys) fixtures. A
count of fixtures says nothing about WHAT they exercise, so this tool walks each fixture's keys
through a small Vim key-language model (Normal / operator-pending / Visual / Insert / Cmdline) and
tags it with every scenario class it touches. One fixture usually belongs to several classes
(`"ayiw` is register + text object). The tagging is HEURISTIC — a reading of the keys, not an
execution — and is meant to show where the corpus is thin, not to certify coverage.

Classes:
  motion         a bare cursor motion in Normal mode
  op_motion      an operator applied over a motion (`dw`, `c$`, `>j`, `gUe`)
  text_object    an `i`/`a` object after an operator or in Visual mode
  visual         Visual, Visual-line or Visual-block mode entered
  insert         Insert or Replace mode entered (`i a o s c R` ...)
  ex             a `:` command line
  register       a named/special register (`"x`), a put, or Insert/Cmdline CTRL-R
  macro          `q` recording or `@` replay
  undo           `u`, `CTRL-R`, `U`, `:undo`/`:redo`
  marks          `m`, `'`, `` ` ``, `:marks`-family Ex, or a mark in an Ex range
  search         `/ ? n N * # gn gN`, `:s`/`:g`
  counts         a count before a command
  error          the oracle recorded an error (`expect.emsg`) or a failed command (`expect.failed`)
  multibyte      non-ASCII buffer text or keys
  edge_buffer    an empty buffer, a blank-only buffer, or a one-character buffer
  dot_repeat     `.`
  config         a fixture `setup` (option-dependent behaviour)

  python3 tools/parity/scenario_classes.py              # summary table + gaps
  python3 tools/parity/scenario_classes.py --markdown   # the same as Markdown (for a PR body)
  python3 tools/parity/scenario_classes.py --list CLASS # the fixtures tagged CLASS

Stdlib only (the corpus is JSON-in-YAML).
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from collections import Counter
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_CORPUS = REPO_ROOT / "tests" / "parity" / "vim" / "fixtures" / "corpus.yaml"

CLASSES = (
    "motion",
    "op_motion",
    "text_object",
    "visual",
    "insert",
    "ex",
    "register",
    "macro",
    "undo",
    "marks",
    "search",
    "counts",
    "error",
    "multibyte",
    "edge_buffer",
    "dot_repeat",
    "config",
)

# A class is THIN below this share of the corpus, or below the absolute floor.
THIN_SHARE = 0.03
THIN_FLOOR = 40

_TOKEN = re.compile(r"<[A-Za-z][-A-Za-z0-9]*>|<[CMAS]-.>|.", re.S)

MOTIONS_1 = set("hjkl0^$wWbBeE(){}GHLM|%_-+ ") | {
    "<Left>", "<Right>", "<Up>", "<Down>", "<Home>", "<End>", "<BS>", "<CR>", "<Space>",
    "<S-Right>", "<S-Left>", "<C-Right>", "<C-Left>", "<C-n>", "<C-p>", "<C-j>", "<C-h>", "<C-m>",
}
MOTIONS_G = set("gejkm0^$_oMEnN'`") | {"<Up>", "<Down>", "<Home>", "<End>"}
CHAR_MOTIONS = set("fFtT")
BRACKET_MOTIONS = set("[]")
SCROLLS = {"<C-f>", "<C-b>", "<C-d>", "<C-u>", "<C-e>", "<C-y>", "<PageUp>", "<PageDown>"}
OPERATORS_1 = set("dcy<>=!")
OPERATORS_G = set("~uU?qw@")
OBJECT_CHARS = set("wWsptbB<>()[]{}\"'`")
INSERT_ENTRY = set("iaIAoOsSCR")


_RAW_CTRL = {"\x16": "<C-v>", "\x12": "<C-r>", "\x1b": "<Esc>", "\r": "<CR>", "\x17": "<C-w>"}


def tokens(keys: str) -> list[str]:
    """Split Vim key notation into keys; a raw control byte reads as its `<C-x>` name."""
    return [_RAW_CTRL.get(t, t) for t in _TOKEN.findall(keys)]


def classify(fx: dict) -> set[str]:
    """Every scenario class one fixture touches."""
    tags: set[str] = set()
    keys = fx["keys"]
    lines = fx["lines"]
    expect = fx.get("expect", {})
    if expect.get("emsg") or expect.get("failed"):
        tags.add("error")
    if any(ord(ch) > 127 for ch in "".join(lines) + keys):
        tags.add("multibyte")
    if lines in ([], [""]) or all(not ln.strip() for ln in lines) or len("".join(lines)) <= 1:
        tags.add("edge_buffer")
    if fx.get("setup"):
        tags.add("config")

    toks = tokens(keys)
    i = 0
    mode = "n"  # n normal, v visual, i insert, c cmdline
    pending_op = False
    cmd: list[str] = []
    cmd_kind = ":"
    cmd_return = "n"
    while i < len(toks):
        if mode == "i":
            tags.add("insert")
        t = toks[i]
        nxt = toks[i + 1] if i + 1 < len(toks) else ""
        if mode == "i":
            if t in ("<Esc>", "<C-c>", "<C-[>"):
                mode = "n"
            elif t == "<C-r>":
                tags.add("register")
                i += 1
            i += 1
            continue
        if mode == "c":
            if t in ("<CR>", "<Esc>", "<C-c>"):
                line = "".join(cmd)
                if cmd_kind == ":":
                    _classify_ex(line, tags)
                else:
                    tags.add("search")
                mode = cmd_return if cmd_kind != ":" else "n"
                if t == "<CR>" and cmd_kind != ":" and pending_op:
                    tags.add("op_motion")
                pending_op = False
                cmd = []
            elif t == "<C-r>":
                tags.add("register")
                cmd.append("\x12")
                i += 1
            else:
                cmd.append(t)
            i += 1
            continue
        # Normal / Visual / operator-pending.
        if t.isdigit() and (t != "0" or (i > 0 and toks[i - 1].isdigit())):
            tags.add("counts")
            i += 1
            continue
        if t == '"' and nxt:
            tags.add("register")
            i += 2
            continue
        if t in (":", "/", "?"):
            if t == ":" and mode == "v":
                tags.add("visual")
            cmd_kind = t
            cmd_return = mode
            mode = "c"
            i += 1
            continue
        if pending_op:
            if t in ("i", "a") and nxt in OBJECT_CHARS:
                tags.add("text_object")
                pending_op = False
                i += 2
                continue
            if t in ("v", "V", "<C-v>"):
                i += 1
                continue  # forced motion type
            if t in ("'", "`") and nxt:
                tags.update({"marks", "op_motion"})
                pending_op = False
                i += 2
                continue
            if t in ("n", "N", "*", "#"):
                tags.update({"search", "op_motion"})
                pending_op = False
                i += 1
                continue
            if t == "g" and nxt in ("n", "N"):
                tags.update({"search", "op_motion"})
                pending_op = False
                i += 2
                continue
            # Any other key is the operator's motion (a doubled `gUU` counts as a line motion).
            tags.add("op_motion")
            pending_op = False
            if t in CHAR_MOTIONS or t in BRACKET_MOTIONS or t == "g" or t == "z":
                i += 2
            else:
                i += 1
            continue
        if mode == "v":
            if t in ("<Esc>", "<C-c>"):
                mode = "n"
                i += 1
                continue
            if t in ("i", "a") and nxt in OBJECT_CHARS:
                tags.add("text_object")
                i += 2
                continue
            # A Visual operator ends the selection; the changes (and block `I`/`A`) enter Insert.
            if t in "dcyxXsSrJpPuU~<>=!CRDYIA":
                if t in "pP":
                    tags.add("register")
                if t == "r":
                    i += 1
                mode = "i" if t in "cCsSRIA" else "n"
                i += 1
                continue
            if t == "g" and nxt:
                if nxt in ("u", "U", "~", "?", "q", "w", "J"):
                    mode = "n"
                i += 2
                continue
            if t in ("'", "`") and nxt:
                tags.add("marks")
                i += 2
                continue
            if t in ("n", "N", "*", "#"):
                tags.add("search")
            if t in CHAR_MOTIONS and nxt:
                i += 2
                continue
            i += 1
            continue
        # Normal mode.
        if t == "<C-w>" and nxt:  # a window command
            i += 2
            continue
        if t in ("v", "V", "<C-v>"):
            tags.add("visual")
            mode = "v"
            i += 1
            continue
        if t == "g" and nxt in ("v", "h", "H", "<C-h>"):
            tags.add("visual")
            mode = "v" if nxt == "v" else "i"  # Select mode types text like Insert
            i += 2
            continue
        if t in OPERATORS_1:
            pending_op = True
            if nxt == t:
                tags.add("op_motion")
                pending_op = False
                i += 2
                if t == "c":
                    mode = "i"
                continue
            i += 1
            if t == "c":
                # `c{motion}` resolves the motion, then enters Insert.
                i = classify_motion_then_insert(toks, i, tags)
                pending_op = False
                mode = "i"
            continue
        if t == "g" and nxt in OPERATORS_G:
            pending_op = True
            i += 2
            continue
        if t == "z" and nxt == "f":
            pending_op = True
            i += 2
            continue
        if t == "q" and nxt and nxt not in (":", "/", "?"):
            tags.add("macro")
            i += 2
            continue
        if t == "@" and nxt:
            tags.add("macro")
            i += 2
            continue
        if t in ("m",) and nxt:
            tags.add("marks")
            i += 2
            continue
        if t in ("'", "`") and nxt:
            tags.add("marks")
            i += 2
            continue
        if t in ("u", "U", "<C-r>"):
            tags.add("undo")
            i += 1
            continue
        if t == "g" and nxt in ("-", "+"):
            tags.add("undo")
            i += 2
            continue
        if t in ("n", "N", "*", "#"):
            tags.add("search")
            i += 1
            continue
        if t == "g" and nxt in ("n", "N", "*", "#"):
            tags.add("search")
            if nxt in ("n", "N"):
                tags.add("visual")
            i += 2
            continue
        if t == ".":
            tags.add("dot_repeat")
            i += 1
            continue
        if t in ("p", "P") or (t == "g" and nxt in ("p", "P")) or (t == "]" and nxt in ("p", "P")):
            tags.add("register")
            i += 1 if t in ("p", "P") else 2
            continue
        if t in ("x", "X", "D", "Y", "J", "~"):
            tags.add("op_motion")  # an operator shorthand (`x` = `dl`, `D` = `d$`)
            i += 1
            continue
        if t in INSERT_ENTRY or (t == "g" and nxt in ("i", "I")):
            tags.add("insert")
            mode = "i"
            i += 1 if t != "g" else 2
            continue
        if t == "r" and nxt:
            i += 2
            continue
        if t in CHAR_MOTIONS and nxt:
            tags.add("motion")
            i += 2
            continue
        if t in (";", ","):
            tags.add("motion")
            i += 1
            continue
        if t in BRACKET_MOTIONS and nxt:
            tags.add("motion")
            i += 2
            continue
        if t == "g" and nxt in MOTIONS_G:
            tags.add("motion")
            i += 2
            continue
        if t in MOTIONS_1 or t in SCROLLS:
            tags.add("motion")
            i += 1
            continue
        if t == "z" and nxt:
            i += 2
            continue
        i += 1
    if mode == "i":
        tags.add("insert")
    return tags


def classify_motion_then_insert(toks: list[str], i: int, tags: set[str]) -> int:
    """After `c`: consume the motion or object, tag it, return the index of the first Insert key."""
    while i < len(toks) and toks[i].isdigit():
        tags.add("counts")
        i += 1
    if i >= len(toks):
        return i
    t = toks[i]
    nxt = toks[i + 1] if i + 1 < len(toks) else ""
    tags.add("insert")
    if t in ("i", "a") and nxt in OBJECT_CHARS:
        tags.add("text_object")
        return i + 2
    tags.add("op_motion")
    if t in ("'", "`"):
        tags.add("marks")
        return i + 2
    if t in ("n", "N", "*", "#"):
        tags.add("search")
        return i + 1
    if t in ("/", "?"):
        tags.add("search")
        j = i + 1
        while j < len(toks) and toks[j] != "<CR>":
            j += 1
        return j + 1
    if t in CHAR_MOTIONS or t in BRACKET_MOTIONS or t == "g":
        return i + 2
    return i + 1


_EX_UNDO = re.compile(r"^\s*(u|un|und|undo|red|redo|undoj|undojoin|earlier|later)\b")
_EX_SEARCH = re.compile(r"(^|[\d,.$%'<>])\s*(s|su|sub|substitute|g|gl|global|v|vg|vglobal|&&?|~)\W")


def _classify_ex(line: str, tags: set[str]) -> None:
    tags.add("ex")
    if _EX_UNDO.match(line):
        tags.add("undo")
    if re.search(r"'[a-z<>\[\]]", line) or re.match(r"\s*(ma|mark|k|marks|delm|delmarks)\b", line):
        tags.add("marks")
    if re.match(r"\s*(norm|normal)\b", line) and "@" in line:
        tags.add("macro")
    if re.search(r"(^|\W)(pu|put|reg|registers|di|display)(\W|$)", line) or "\x12" in line:
        tags.add("register")
    if re.search(r"(^|\W)(y|ya|yank|d|de|del|delete)\s+[a-zA-Z\"]", line):
        tags.add("register")
    if _EX_SEARCH.search(line) or line.lstrip().startswith(("s/", "g/", "v/", "%s", "%g")):
        tags.add("search")


def summarize(fixtures: list[dict]) -> dict:
    per_class: Counter[str] = Counter()
    error_cross: Counter[str] = Counter()
    for fx in fixtures:
        tags = classify(fx)
        for c in tags:
            per_class[c] += 1
        if "error" in tags:
            for c in tags - {"error"}:
                error_cross[c] += 1
    total = len(fixtures)
    floor = max(THIN_FLOOR, int(total * THIN_SHARE))
    thin = [c for c in CLASSES if per_class[c] < floor]
    return {
        "total": total,
        "floor": floor,
        "per_class": per_class,
        "thin": thin,
        "error_cross": error_cross,
        "with_emsg": sum(1 for fx in fixtures if fx.get("expect", {}).get("emsg")),
        "with_failed": sum(1 for fx in fixtures if fx.get("expect", {}).get("failed")),
        "max_keys": max((len(tokens(fx["keys"])) for fx in fixtures), default=0),
        "max_lines": max((len(fx["lines"]) for fx in fixtures), default=0),
    }


def render(s: dict, markdown: bool) -> str:
    out = []
    total = s["total"]
    if markdown:
        out.append(f"Corpus: {total} fixtures; a class is thin below {s['floor']} fixtures.\n")
        out.append("| class | fixtures | share | of which error |")
        out.append("|---|---:|---:|---:|")
        for c in CLASSES:
            n = s["per_class"][c]
            err = n if c == "error" else s["error_cross"][c]
            mark = " (thin)" if c in s["thin"] else ""
            out.append(f"| {c}{mark} | {n} | {100 * n / max(total, 1):.1f}% | {err} |")
    else:
        out.append(f"corpus: {total} fixtures (thin floor: {s['floor']})")
        out.append(f"{'class':<14} {'fixtures':>8} {'share':>7} {'error':>6}")
        for c in CLASSES:
            n = s["per_class"][c]
            err = n if c == "error" else s["error_cross"][c]
            mark = "  THIN" if c in s["thin"] else ""
            out.append(f"{c:<14} {n:>8} {100 * n / max(total, 1):>6.1f}% {err:>6}{mark}")
    out.append("")
    out.append(
        f"error observables: {s['with_emsg']} fixtures raise an E-number (emsg), "
        f"{s['with_failed']} fail a command (failed); longest keys {s['max_keys']} tokens, "
        f"largest buffer {s['max_lines']} lines."
    )
    out.append("thin classes: " + (", ".join(s["thin"]) if s["thin"] else "none"))
    return "\n".join(out)


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--corpus", type=Path, default=DEFAULT_CORPUS)
    ap.add_argument("--markdown", action="store_true")
    ap.add_argument("--list", metavar="CLASS", choices=CLASSES)
    args = ap.parse_args(argv[1:])
    corpus = json.loads(args.corpus.read_text(encoding="utf-8"))
    fixtures = corpus["fixtures"]
    if args.list:
        for fx in fixtures:
            if args.list in classify(fx):
                print(f"{fx['name']:<48} {fx['keys']!r}")
        return 0
    print(render(summarize(fixtures), args.markdown))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
