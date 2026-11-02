#!/usr/bin/env python3
"""Neovim pattern oracle — differential ground truth for ruse's Vim-regex front-end (issue #692).

tools/parity/oracle.py captures what KEYS do; this harness captures what a PATTERN matches. For each
(pattern, subject, case options) it asks the pinned Neovim where the first match is, and writes the
answers into a generated Rust table (crates/core/tests/pattern_oracle/cases.rs) that
crates/core/tests/pattern_oracle.rs replays against `ruse_core::Regex::compile` + `find_at`.

HOW A MATCH IS OBSERVED (read-only, one fresh `nvim -u NONE -i NONE` process for the whole table):
    - A one-line subject, no 'smartcase': `matchstrpos(subject, pattern)` — the byte span [start, end)
      of the first match, or no match. This is string matching, which is what a substitute sees per line.
    - A multi-line subject, or 'smartcase' (which `matchstrpos` ignores): the subject is put in a buffer
      and `searchpos(pattern, 'cnW')` / `searchpos(pattern, 'cenW')` from line 1 col 0 give the start and
      the last character of the match; the span is converted to byte offsets in the lines joined by
      "\\n" — ruse's buffer haystack. Multi-line cases are written to match at least one character (the
      'e' flag cannot tell an empty match from a one-character one).
    - A Vim error (E-number) is recorded as `Error`; ruse must then also refuse the pattern.

EXPECTATIONS. Every row asserts ruse agrees with nvim, except rows marked `unsupported`: atoms the
wrapped Rust engine cannot express (lookaround, backrefs, `\\&`, `~`, position atoms, ...). For those
ruse must return `RegexError::Unsupported` — rejected, never faked — and nvim's answer is recorded for
the reader.

The pin is verified exactly as oracle.py verifies it (spec/parity/upstreams.yaml neovim.version_label).
Stdlib only (D-034).

Usage:
    python3 tools/parity/pattern_oracle.py            # regenerate the table
    python3 tools/parity/pattern_oracle.py --check    # fail if the committed table is stale
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from oracle import OracleError, assert_pin, nvim_version, read_pin  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parents[2]
TABLE = REPO_ROOT / "crates" / "core" / "tests" / "pattern_oracle" / "cases.rs"

# (pattern, subject, options, expectation). `subject` is a str (one line) or a list of lines. `options`
# is "" | "ic" | "ic scs". `expectation` is "" (ruse agrees with nvim) or "unsupported".
U = "unsupported"
CASES: list[tuple[str, str | list[str], str, str]] = [
    # --- #692: bracket ranges and contents ---------------------------------------------------------
    (r"[0-9]", "a-1-2", "", ""),
    (r"[a-z]\+", "Ab-cd", "", ""),
    (r"[0-9]", "a-b", "", ""),
    (r"[0-9]", "123", "", ""),
    (r"[^x]", "xxy", "", ""),
    (r"[\d]", "d5", "", ""),
    (r"[\d]", "\\", "", ""),
    (r"[\de]", "e", "", ""),
    (r"[]abc]", "x]", "", ""),
    (r"[^]a]", "]ab", "", ""),
    (r"[[]", "a[", "", ""),
    (r"[a--b]", "-", "", ""),
    (r"[a--b]", "b", "", ""),
    (r"[a-c-e]", "d-", "", ""),
    (r"[--a]", "+", "", ""),
    (r"[a-]", "-", "", ""),
    (r"[-a]", "-", "", ""),
    (r"[b-a]", "a", "", ""),
    (r"[\x61]", "xa", "", ""),
    (r"[\d97-z]", "b", "", ""),
    (r"[a-\d99]", "b", "", ""),
    (r"[\o141]", "a", "", ""),
    (r"[é]", "é", "", ""),
    (r"[\e]", "\x1b", "", ""),
    (r"[\t]", "\t", "", ""),
    (r"[\]]", "]", "", ""),
    (r"[\^]", "^", "", ""),
    (r"[\-]", "-", "", ""),
    (r"[a\-z]", "b", "", ""),
    (r"[\\]", "\\", "", ""),
    (r"[\s]", "s", "", ""),
    (r"[\x]", "x", "", ""),
    (r"[[:alpha:]]", "1a", "", ""),
    (r"[[:alpha:]]", "é", "", ""),
    (r"[[:upper:]]", "aÉ", "", ""),
    (r"[[:lower:]]", "Aé", "", ""),
    (r"[[:digit:]x]", "ax", "", ""),
    (r"[[:space:]]", "a\tb", "", ""),
    (r"[[:punct:]]", "a,b", "", ""),
    (r"[[:xdigit:]]\+", "xbeefy", "", ""),
    (r"[[:ident:]]\+", "-é_9-", "", ""),
    (r"[[:keyword:]]", "-中", "", ""),
    (r"[[:fname:]]\+", " a/b.c ", "", ""),
    (r"[[:return:]]", "a\rb", "", ""),
    (r"[[:tab:]]", "a\tb", "", ""),
    (r"[[:bogus:]]", "b", "", ""),
    (r"[[:alpha:]-z]", "-", "", ""),
    (r"[a-[:alpha:]]", "b", "", ""),
    (r"[[.a.]]", "a", "", ""),
    (r"[[.a.]b]", "b", "", ""),
    (r"[[=a=]]", "a", "", U),
    (r"[a-z]", "é", "", ""),
    (r"\v[(]\zsfoo", "(foo", "", ""),
    (r"foo[1", "foo[1", "", ""),
    (r"[", "[", "", ""),
    (r"\_[ab", "_[ab", "", ""),
    # --- #692: non-anchor ^ $ * ------------------------------------------------------------------
    (r"$foo", "cost $foo", "", ""),
    (r"a^b", "a^b", "", ""),
    (r"a$b", "a$b", "", ""),
    (r"*foo", "*foo", "", ""),
    (r"\(*a\)", "*a", "", ""),
    (r"^*x", "*x", "", ""),
    (r"^\(*a\)", "*a", "", ""),
    (r"\(^a\)", "ba", "", ""),
    (r"\(^a\)", "a", "", ""),
    (r"b\|^a", "a", "", ""),
    (r"a\|^b", "b", "", ""),
    (r"a\(*\)", "a*", "", ""),
    (r"\(a\|*\)", "*", "", ""),
    (r"a$\|b", "a", "", ""),
    (r"\(a$\)", "a", "", ""),
    (r"a$\c", "a", "", ""),
    (r"$\|a", "x", "", ""),
    (r"a$\v|b", "a", "", ""),
    (r"x\$", "x$", "", ""),
    (r"\%(^b\)", "b", "", ""),
    (r"a\%(^b\)", "ab", "", ""),
    (r"\c^*a", "*A", "", ""),
    (r"^\*a", "*a", "", ""),
    (r"a**", "aa", "", ""),
    (r"^b", ["a", "b"], "", ""),
    (r"a$", ["xa", "b"], "", ""),
    (r"a\nb", ["xa", "b"], "", ""),
    (r"\n^b", ["a", "b"], "", ""),
    # --- #692: escapes honour the magic level ----------------------------------------------------
    (r"\v\(a\)", "(a)", "", ""),
    (r"\v\+", "+", "", ""),
    (r"\v\?", "?", "", ""),
    (r"\v\{", "{", "", ""),
    (r"\v\|", "a|", "", ""),
    (r"\v\=", "=", "", ""),
    (r"\v\<", "<", "", ""),
    (r"\v\>", ">", "", ""),
    (r"\v\.", "a.", "", ""),
    (r"\v\*", "*", "", ""),
    (r"\v\[", "[", "", ""),
    (r"\v\%", "%", "", ""),
    (r"\v\@", "@", "", ""),
    (r"\v\}", "}", "", ""),
    (r"\v}", "}", "", ""),
    (r"\v-", "a-b", "", ""),
    (r"\v!", "a!b", "", ""),
    (r"\va^b", "a^b", "", ""),
    (r"\v$a", "$a", "", ""),
    (r"\v^a", "a", "", ""),
    (r"\v^*", "*", "", ""),
    (r"\v(*)", "*", "", ""),
    (r"\v(^a)", "a", "", ""),
    (r"\va{2}", "aaa", "", ""),
    (r"\va{-}", "aa", "", ""),
    (r"\va{-1,}", "aaa", "", ""),
    (r"\v(ab)+", "zababz", "", ""),
    (r"\va|b", "b", "", ""),
    (r"\va=b", "ab", "", ""),
    (r"\va=b", "b", "", ""),
    (r"\va?b", "b", "", ""),
    (r"\v=", "a=", "", ""),
    (r"\v{", "{", "", ""),
    (r"\v<foo>", "foo foobar", "", ""),
    (r"\v<foo>", "foobar foo", "", ""),
    (r"\v%(ab)+", "abab", "", ""),
    (r"\v%[ab]", "ab", "", ""),
    (r"\vfoo@=", "foo", "", U),
    (r"\va&", "a&", "", U),
    (r"\v~", "a~", "", U),
    (r"\Ma\.b", "axb", "", ""),
    (r"\Ma.b", "axb", "", ""),
    (r"\Ma.b", "a.b", "", ""),
    (r"\Ma\*", "aaa", "", ""),
    (r"\Ma*", "a*", "", ""),
    (r"\M\[a]", "a", "", ""),
    (r"\M[a]", "[a]", "", ""),
    (r"\M^*", "*", "", ""),
    (r"\M\*", "a*", "", ""),
    (r"\M~", "a~", "", ""),
    (r"\Ma\{2}", "aa", "", ""),
    (r"\Va\.b", "axb", "", ""),
    (r"\Va.b", "axb", "", ""),
    (r"\Va\*", "aaa", "", ""),
    (r"\V\^a", "a", "", ""),
    (r"\V^a", "^a", "", ""),
    (r"\Va\$", "a", "", ""),
    (r"\Va$", "a$", "", ""),
    (r"\V$a", "$a", "", ""),
    (r"\V\$a", "$a", "", ""),
    (r"\Va\|b", "b", "", ""),
    (r"\V[a]", "[a]", "", ""),
    (r"\V\[a]", "a", "", ""),
    (r"\va+\m.", "aax", "", ""),
    (r"\Va.\m.", "a.x", "", ""),
    # --- magic default, backslash forms --------------------------------------------------------
    (r"f(x", "a f(x b", "", ""),
    (r"a+", "xa+y", "", ""),
    (r"a\+", "xaaay", "", ""),
    (r"\(ab\)\+", "zababz", "", ""),
    (r"a\=b", "b", "", ""),
    (r"a\?b", "ab", "", ""),
    (r"a{", "a{", "", ""),
    (r"}", "}", "", ""),
    (r"\}", "}", "", ""),
    (r"a\{2}", "aaa", "", ""),
    (r"a\{,2}", "aaa", "", ""),
    (r"a\{2,}", "aaaa", "", ""),
    (r"x\{3,1}", "xxxx", "", ""),
    (r"a\{-2,}", "aaaa", "", ""),
    (r"a\{-,2}", "aaa", "", ""),
    (r"a\{-2}", "aaa", "", ""),
    (r"a\{0}b", "ab", "", ""),
    (r"a\{\}", "aa", "", ""),
    (r"a\{-}", "aa", "", ""),
    (r"a.\{-}b", "axbxb", "", ""),
    (r"a\{1,2\}", "aa", "", ""),
    (r"a\{ 1}", "aa", "", ""),
    (r"a\{", "a{", "", ""),
    (r"a\{x}", "a{x}", "", ""),
    (r"a\{1,2", "aa", "", ""),
    (r"\{", "{", "", ""),
    (r"\(a", "a", "", ""),
    (r"a\)", "a", "", ""),
    (r"\%(a", "a", "", ""),
    (r"\%(ab\)\+", "xabab", "", ""),
    (r"\%[abc]x", "abx", "", ""),
    (r"r\%[ead]", "rea", "", ""),
    (r"\%[r\%[ab]]", "rab", "", ""),
    (r"\%[a[bc]]x", "acx", "", ""),
    (r"\%[a\(b\)]", "ab", "", ""),
    (r"\%[ab]*", "abab", "", ""),
    (r"\%[a]\+", "aa", "", ""),
    (r"\%[ab", "a", "", ""),
    (r"\%[]", "a", "", ""),
    (r"\%d97", "xa", "", ""),
    (r"\%x61", "xa", "", ""),
    (r"\%o141", "a", "", ""),
    (r"\%u00e9", "é", "", ""),
    (r"\%U000000e9", "é", "", ""),
    (r"\%d", "d", "", ""),
    (r"\%q", "q", "", ""),
    (r"\%#=1a", "a", "", ""),
    (r"\%#=2a", "a", "", ""),
    (r"a\%#=1", "a", "", ""),
    (r"\-", "-", "", ""),
    (r"\/", "/", "", ""),
    (r"\,", ",", "", ""),
    (r"\]", "]", "", ""),
    (r"\q", "q", "", ""),
    (r"\Q", "q", "", ""),
    (r"\y", "y", "", ""),
    (r"\0", "0", "", ""),
    (r"\\", "\\", "", ""),
    (r"\\", "a\\", "", ""),
    (r"a\\", "a\\", "", ""),
    (r"\_", "_", "", ""),
    (r"\_q", "q", "", ""),
    (r"\z", "z", "", ""),
    (r"\zq", "z", "", ""),
    (r"\z(a\)", "a", "", ""),
    (r"\z1", "a", "", ""),
    (r"\zs*", "a", "", ""),
    # --- #692: \c \C, case classes under 'ignorecase' --------------------------------------------
    (r"\cfoo", "FOO", "", ""),
    (r"foo\c", "xFoO", "", ""),
    (r"\Cfoo", "FOO", "ic", ""),
    (r"\C\cfoo", "FOO", "ic", ""),
    (r"\c\Cfoo", "FOO", "", ""),
    (r"\Ca\cA", "aa", "", ""),
    (r"foo", "FOO", "ic", ""),
    (r"Foo", "foo", "ic scs", ""),
    (r"foo", "FOO", "ic scs", ""),
    (r"\Sfoo", "xFOO", "ic scs", ""),
    (r"\c\u", "a", "", ""),
    (r"\c\l", "A", "", ""),
    (r"\c\U", "a", "", ""),
    (r"\c\L", "A", "", ""),
    (r"\c\a", "A", "", ""),
    (r"\c[A-Z]", "a", "", ""),
    (r"\c[^a]", "A", "", ""),
    (r"\c[[:upper:]]", "a", "", ""),
    (r"\c[[:upper:]x]", "aX", "", ""),
    (r"\c[^[:upper:]]", "a", "", U),
    (r"\u", "a", "ic", ""),
    (r"[A-Z]", "a", "ic", ""),
    # Vim never case-folds a class atom or a recognised class spelling, so no Unicode fold sneaks in
    # (U+212A KELVIN SIGN folds to k, U+017F LONG S to s); other collections fold like literals.
    (r"\a", "K", "ic", ""),
    (r"\w", "ſ", "ic", ""),
    (r"\W", "K", "ic", ""),
    (r"\h", "K", "ic", ""),
    (r"\i", "ſ", "ic", ""),
    (r"\i", "Ÿ", "ic", ""),
    (r"\k", "Ÿ", "ic", ""),
    (r"\x", "K", "ic", ""),
    (r"k", "K", "ic", ""),
    (r"[k]", "K", "ic", ""),
    (r"[s]", "ſ", "ic", ""),
    (r"[a-z]", "K", "ic", ""),
    (r"[a-z]", "K", "ic", ""),
    (r"[a-z]", "K", "", ""),
    (r"[A-Z]", "k", "ic", ""),
    (r"[^a-z]", "K", "ic", ""),
    (r"[^a-z]", "K", "ic", ""),
    (r"[a-zA-Z]", "K", "ic", ""),
    (r"[A-Za-z_]", "K", "ic", ""),
    (r"[a-zA-Z0-9_]", "ſ", "ic", ""),
    (r"[0-9a-fA-F]", "K", "ic", ""),
    (r"[a-z_]", "K", "ic", ""),
    (r"[a-z0-9]", "K", "ic", ""),
    (r"[a-y]", "K", "ic", ""),
    (r"[j-l]", "K", "ic", ""),
    (r"[^j-l]", "K", "ic", ""),
    (r"[r-t]", "ſ", "ic", ""),
    (r"[a-zé]", "K", "ic", ""),
    (r"[[:alpha:]]", "K", "ic", ""),
    (r"[[:alpha:]]", "A", "ic", ""),
    (r"[[:alpha:]x]", "KX", "ic", ""),
    (r"[[:digit:]k]", "K", "ic", ""),
    (r"[^[:alpha:]]", "K", "ic", U),
    (r"[^[:digit:]]", "K", "ic", ""),
    (r"[a-z\n]", ["1", "a"], "", ""),
    (r"[^a-z]", ["a", "b"], "", ""),
    # --- #692: \_x newline-inclusive forms -------------------------------------------------------
    (r"\_s", ["a", "b"], "", ""),
    (r"a\_s\+b", ["a ", " b"], "", ""),
    (r"a\_.b", ["a", "b"], "", ""),
    (r"a.b", ["a", "b"], "", ""),
    (r"a\_[xy]b", ["a", "b"], "", ""),
    (r"a[^x]b", ["a", "b"], "", ""),
    (r"a\_[^x]b", ["a", "b"], "", ""),
    (r"a\_^b", "ab", "", ""),
    (r"a\_$", "a", "", ""),
    (r"\_^a", "a", "", ""),
    (r"\_x", "g", "", ""),
    # (a following line: in a buffer, `\n` also matches at the end of the LAST line, which a joined
    # haystack without a trailing "\n" cannot show)
    (r"\_a\+", ["xa", "b", "-"], "", ""),
    (r"a\S", ["a", "b"], "", ""),
    (r"a\_S", ["a", "b"], "", ""),
    (r"\_[ab]", "_", "", ""),
    (r"a[\n]b", ["a", "b"], "", ""),
    (r"a[^\n]", ["a", "b"], "", ""),
    (r"\_[^\n]", "x", "", ""),
    (r"a\n", ["a", "b"], "", ""),
    # --- #692: \e \t \r \n escapes ---------------------------------------------------------------
    (r"\e", "a\x1b", "", ""),
    (r"\t", "a\tb", "", ""),
    (r"\r", "a\rb", "", ""),
    (r"\b", "a\x08b", "", ""),
    (r"\n", "x", "", ""),
    # --- #692: character classes ------------------------------------------------------------------
    (r"\i\+", "-é_9µ-", "", ""),
    (r"\i", "中", "", ""),
    (r"\I", "1a", "", ""),
    (r"\k\+", "-中文-", "", ""),
    (r"\k", "—", "", ""),
    (r"\k", "😀", "", ""),
    (r"\k\+", "a×b", "", ""),
    (r"\K", "1a", "", ""),
    (r"\f\+", " /a/b.c~ ", "", ""),
    (r"\f", "中", "", ""),
    (r"\F", "1a", "", ""),
    (r"\p\+", "\x01ab\x7f", "", ""),
    (r"\p", "é", "", ""),
    (r"\p", "\t", "", ""),
    (r"\p", "​", "", ""),
    (r"\P", "1a", "", ""),
    (r"\s\+", "a \tb", "", ""),
    (r"\S", " a", "", ""),
    (r"\d\+", "x123y", "", ""),
    (r"\d", "٣", "", ""),
    (r"\D", "1a", "", ""),
    (r"\w\+", "-ab_9-", "", ""),
    (r"\w", "é", "", ""),
    (r"\W", "a-", "", ""),
    (r"\a", "1é", "", ""),
    (r"\A", "a1", "", ""),
    (r"\l", "Aa", "", ""),
    (r"\L", "aA", "", ""),
    (r"\u", "aA", "", ""),
    (r"\U", "Aa", "", ""),
    (r"\h\+", "1a_b", "", ""),
    (r"\H", "_1", "", ""),
    (r"\o\+", "98071", "", ""),
    (r"\O", "78", "", ""),
    (r"\x\+", "xbeefg", "", ""),
    (r"\X", "fg", "", ""),
    (r".", "é", "", ""),
    # --- word boundaries --------------------------------------------------------------------------
    (r"\<foo\>", "foobar foo", "", ""),
    (r"\<", "a", "", ""),
    (r"a\>", "ab a", "", ""),
    (r"\<é", "aé é", "", ""),
    # --- \zs / \ze (#738 / #1024 fall out of the empty-group markers) ----------------------------
    (r"foo\zsbar", "a foobar b", "", ""),
    (r"foo\zsbar", "a xxxbar b", "", ""),
    (r"o\zsba\zer", "a foobar b", "", ""),
    (r"foo\zebar", "a foobar b", "", ""),
    (r"a\zsb\zsc", "abc", "", ""),
    (r"a\zeb\zec", "abc", "", ""),
    (r"a\zeb\zsc", "abc", "", ""),
    (r"foo\zsbar\|baz", "baz", "", ""),
    (r"foo\|bar\zebaz", "barbaz", "", ""),
    (r"\(a\zsb\)*", "abab", "", ""),
    (r"\(ab\)*\zsc", "ababc", "", ""),
    # `\=` / `\?` on a marker are accepted (only the repeating multis are E888)
    (r"a\zs\=b", "ab", "", ""),
    (r"a\zs\?b", "ab", "", ""),
    (r"a\ze\=b", "ab", "", ""),
    (r"a\ze\?b", "ab", "", ""),
    (r"a\zs\+b", "ab", "", ""),
    (r"a\ze\{2}b", "ab", "", ""),
    # --- rejected, never faked ---------------------------------------------------------------------
    (r"\(a\)\1", "aa", "", U),
    (r"foo\@=", "foo", "", U),
    (r"\(foo\)\@<=bar", "foobar", "", U),
    (r"\(a\)\@>", "a", "", U),
    (r"a\&", "a&", "", U),
    (r"~", "a~", "", U),
    (r"\%Va", "a", "", U),
    (r"\%2c", "ab", "", U),
    (r"\%1la", "a", "", U),
    (r"\%^a", "a", "", U),
    (r"a\%$", "a", "", U),
    (r"a\%#", "a", "", U),
    (r"\Za", "a", "", U),
    (r"\1", "1", "", ""),
]

_LUA = r"""
local cases = vim.json.decode(io.open(%s):read('*a'))
local out = {}
local function off(lines, l, c)  -- 1-based line, 0-based byte col -> byte offset in joined text
  local n = 0
  for i = 1, l - 1 do n = n + #lines[i] + 1 end
  return n + c
end
for _, k in ipairs(cases) do
  vim.o.ignorecase = k.ic
  vim.o.smartcase = k.scs
  local r
  if #k.lines == 1 and not k.scs then
    local ok, m = pcall(vim.fn.matchstrpos, k.lines[1], k.pat)
    if not ok then r = { kind = 'error', msg = tostring(m) }
    elseif m[2] < 0 then r = { kind = 'none' }
    else r = { kind = 'match', s = m[2], e = m[3] } end
  else
    vim.api.nvim_buf_set_lines(0, 0, -1, false, k.lines)
    vim.api.nvim_win_set_cursor(0, {1, 0})
    local ok, s = pcall(vim.fn.searchpos, k.pat, 'cnW')
    if not ok then r = { kind = 'error', msg = tostring(s) }
    elseif s[1] == 0 then r = { kind = 'none' }
    else
      local e = vim.fn.searchpos(k.pat, 'cenW')
      local line = k.lines[e[1]]
      local len = 1
      if e[2] - 1 < #line then
        local b = line:byte(e[2])
        len = (b < 0x80 and 1) or (b < 0xE0 and 2) or (b < 0xF0 and 3) or 4
      end
      r = { kind = 'match', s = off(k.lines, s[1], s[2] - 1), e = off(k.lines, e[1], e[2] - 1) + len }
    end
  end
  table.insert(out, r)
end
io.write(vim.json.encode(out))
"""


def capture() -> tuple[list[dict], str]:
    """Run every case through one fresh pinned nvim; return the observations and the version line."""
    pin = read_pin()
    version = nvim_version()
    assert_pin(version, pin)
    payload = [
        {
            "pat": pat,
            "lines": [subj] if isinstance(subj, str) else subj,
            "ic": "ic" in opts.split(),
            "scs": "scs" in opts.split(),
        }
        for pat, subj, opts, _ in CASES
    ]
    with tempfile.TemporaryDirectory() as tmp:
        data = Path(tmp) / "cases.json"
        data.write_text(json.dumps(payload), encoding="utf-8")
        script = Path(tmp) / "probe.lua"
        script.write_text(_LUA % json.dumps(str(data)), encoding="utf-8")
        proc = subprocess.run(
            ["nvim", "--headless", "-u", "NONE", "-i", "NONE", "-l", str(script)],
            capture_output=True,
            text=True,
            timeout=120,
        )
    if proc.returncode != 0 or not proc.stdout:
        raise OracleError(f"nvim failed (rc={proc.returncode}): {proc.stderr.strip()}")
    results = json.loads(proc.stdout)
    if len(results) != len(CASES):
        raise OracleError(f"expected {len(CASES)} observations, got {len(results)}")
    return results, version


def rust_str(s: str) -> str:
    """A Rust string literal for `s`; control characters escaped, everything else verbatim."""
    out = []
    for ch in s:
        if ch == "\\":
            out.append("\\\\")
        elif ch == '"':
            out.append('\\"')
        elif ch == "\n":
            out.append("\\n")
        elif ch == "\t":
            out.append("\\t")
        elif ch == "\r":
            out.append("\\r")
        elif ord(ch) < 0x20 or ord(ch) == 0x7F or 0x200B <= ord(ch) <= 0x200F:
            out.append(f"\\u{{{ord(ch):x}}}")
        else:
            out.append(ch)
    return '"' + "".join(out) + '"'


def render(results: list[dict], version: str) -> str:
    pin = read_pin()
    lines = [
        "// GENERATED by tools/parity/pattern_oracle.py — do not edit by hand. Every `nvim` answer was",
        f"// captured from the pinned Neovim ({version}, revision {pin['revision']});",
        "// regenerate with `python3 tools/parity/pattern_oracle.py`.",
        "",
        "use super::{Case, Expect, Nvim};",
        "",
        "#[rustfmt::skip] // one row per line, as generated",
        "pub(crate) const CASES: &[Case] = &[",
    ]
    for (pat, subj, opts, expect), r in zip(CASES, results):
        hay = subj if isinstance(subj, str) else "\n".join(subj)
        if r["kind"] == "match":
            nv = f"Nvim::Match({r['s']}, {r['e']})"
        elif r["kind"] == "none":
            nv = "Nvim::NoMatch"
        else:
            nv = "Nvim::Error"
        ex = "Expect::Unsupported" if expect == U else "Expect::Agree"
        lines.append(
            f"    Case {{ pat: {rust_str(pat)}, hay: {rust_str(hay)}, "
            f"ic: {str('ic' in opts.split()).lower()}, scs: {str('scs' in opts.split()).lower()}, "
            f"nvim: {nv}, expect: {ex} }},"
        )
    lines.append("];")
    lines.append("")
    return "\n".join(lines)


def main(argv: list[str]) -> int:
    try:
        results, version = capture()
    except OracleError as exc:
        print(f"pattern_oracle: {exc}", file=sys.stderr)
        return 2
    text = render(results, version)
    if "--check" in argv:
        current = TABLE.read_text(encoding="utf-8") if TABLE.exists() else ""
        if current != text:
            print(f"pattern_oracle: {TABLE} is stale; regenerate it", file=sys.stderr)
            return 1
        print(f"pattern_oracle: {len(CASES)} cases up to date")
        return 0
    TABLE.parent.mkdir(parents=True, exist_ok=True)
    TABLE.write_text(text, encoding="utf-8")
    print(f"pattern_oracle: wrote {len(CASES)} cases to {TABLE.relative_to(REPO_ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
