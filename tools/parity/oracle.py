#!/usr/bin/env python3
"""Neovim differential oracle — the executable half of the parity verification axis.

This is the FIRST slice of ruse's parity verification: a harness that runs the pinned Neovim as a
black box, feeds a fixture's keystrokes into a scratch buffer, and reads back the resulting editor
state. The captured state becomes the `expect` of a fixture; a fixture is a hand-verified claim about
what Vim's editing language *does*, pinned to an exact upstream revision (spec/parity/upstreams.yaml).

WHY THE SELFTEST GATES EVERYTHING (spec/parity/upstreams.yaml#oracle_selftest):
    Three prior oracle harnesses each corrupted their own first observation — `vim -es` reported the
    HARNESS's mode instead of the fixture's; `emacs --batch execute-kbd-macro` left the buffer empty
    while the kill-ring was correct; `emacs --batch read-from-minibuffer` hung forever. Oracle risk
    exceeds extractor risk, so no fixture is trusted until `--selftest` reproduces cases whose answers
    are known independently of the harness.

THE NON-CORRUPTION TECHNIQUE (this harness's answer to that hazard):
    1. Observation is READ-ONLY and happens AFTER mutation, never through a mutating call. Keys are
       fed with `nvim_feedkeys(nvim_replace_termcodes(keys, true, false, true), 'x', false)` — mode
       'x' executes synchronously and RETURNS, so every read (`nvim_buf_get_lines`,
       `nvim_win_get_cursor`, `getreg`/`getregtype`) runs against already-settled state. No read
       command is itself an editing command (the `vim -es` / `execute-kbd-macro` trap).
    2. One fresh `nvim` PROCESS per fixture — `-u NONE -i NONE` (no user config, no shada). State
       cannot leak between fixtures because there is no shared process to leak through.
    3. The pin is VERIFIED, not assumed. `nvim --version` is parsed and asserted equal to the
       `version_label` recorded in spec/parity/upstreams.yaml, and that version string is recorded in
       every emitted document. A wrong binary refuses rather than silently recording a lie.

Stdlib only (D-034): no PyYAML, no pip deps. The fixture corpus is emitted as JSON, which is a strict
subset of YAML 1.2 — so `corpus.yaml` is valid YAML *and* parses with `serde_json` on the Rust side
with no YAML dependency on either end.

Usage:
    python3 tools/parity/oracle.py --selftest          # prove non-corruption + determinism; gates the corpus
    python3 tools/parity/oracle.py --generate [PATH]    # capture the fixture corpus from the oracle
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
UPSTREAMS = REPO_ROOT / "spec" / "parity" / "upstreams.yaml"
DEFAULT_CORPUS = REPO_ROOT / "tests" / "parity" / "vim" / "fixtures" / "corpus.yaml"

INVOKE = "nvim --headless -u NONE -i NONE -l <script>"

# The Lua probe. It sets the buffer, homes the cursor, applies the fixture's optional `setup` ex command,
# feeds the keys SYNCHRONOUSLY, then reads state. Every line after feedkeys is a pure read — this ordering
# is the non-corruption guarantee. `setup` runs BEFORE feedkeys so it configures the very edit under test
# (see the shift_right_line fixture: config-dependent ops need a config-matched oracle run).
_LUA = r"""
local input = vim.json.decode([==[ %s ]==])
vim.api.nvim_buf_set_lines(0, 0, -1, false, input.lines)
vim.api.nvim_win_set_cursor(0, {1, 0})
if input.setup ~= nil and input.setup ~= '' then
  vim.cmd(input.setup)
end
local tc = vim.api.nvim_replace_termcodes(input.keys, true, false, true)
vim.api.nvim_feedkeys(tc, 'x', false)
io.write(vim.json.encode({
  text        = vim.api.nvim_buf_get_lines(0, 0, -1, false),
  cursor      = vim.api.nvim_win_get_cursor(0),
  reg_unnamed = { vim.fn.getreg('"'), vim.fn.getregtype('"') },
  reg0        = { vim.fn.getreg('0'), vim.fn.getregtype('0') },
  mode        = vim.api.nvim_get_mode().mode,
}))
"""


class OracleError(RuntimeError):
    """The harness cannot make a trustworthy observation (bad binary, version mismatch, nvim error)."""


def read_pin() -> dict[str, str]:
    """Extract Neovim's pinned revision + version_label from spec/parity/upstreams.yaml.

    Parsed by hand (stdlib only): locate the `neovim:` block under `upstreams:` and pull its two
    scalar fields. We deliberately do NOT depend on PyYAML here — the oracle must run with nothing
    installed, and a two-field lookup does not justify a parser dependency.
    """
    text = UPSTREAMS.read_text(encoding="utf-8")
    lines = text.splitlines()
    try:
        start = next(i for i, ln in enumerate(lines) if ln.rstrip() == "upstreams:")
    except StopIteration as exc:  # pragma: no cover - the file always has this key
        raise OracleError(f"no `upstreams:` section in {UPSTREAMS}") from exc
    # The first 2-space-indented `neovim:` after `upstreams:`.
    nv = next((i for i in range(start + 1, len(lines)) if lines[i] == "  neovim:"), None)
    if nv is None:
        raise OracleError("no `neovim:` upstream block")
    rev = ver = None
    for ln in lines[nv + 1 :]:
        # Stop at the next upstream (2-space key) or a top-level key (dedent).
        if re.match(r"^  \S", ln) or re.match(r"^\S", ln):
            break
        m = re.match(r"\s*revision:\s*(\S+)", ln)
        if m and rev is None:
            rev = m.group(1)
        m = re.match(r"\s*version_label:\s*(\S+)", ln)
        if m and ver is None:
            ver = m.group(1)
    if not rev or not ver:
        raise OracleError("neovim block missing revision/version_label")
    return {"revision": rev, "version_label": ver}


def nvim_version() -> str:
    """The first line of `nvim --version`, e.g. 'NVIM v0.12.4'. Recorded in every run."""
    try:
        out = subprocess.run(
            ["nvim", "--version"], capture_output=True, text=True, check=True
        )
    except (OSError, subprocess.CalledProcessError) as exc:
        raise OracleError(f"cannot run `nvim --version`: {exc}") from exc
    return out.stdout.splitlines()[0].strip()


def assert_pin(version_line: str, pin: dict[str, str]) -> None:
    """Refuse to observe through a binary that is not the pinned revision (contract: warn/refuse).

    We match the `version_label` (e.g. 'v0.12.4') as a token in `nvim --version`. A release binary
    does not expose the peeled commit sha, so the label is the strongest check available here; the
    sha is still recorded in the emitted document so the claim is auditable.
    """
    label = pin["version_label"]
    if not re.search(rf"\b{re.escape(label)}\b", version_line):
        raise OracleError(
            f"nvim version mismatch: `{version_line}` is not the pinned {label} "
            f"(spec/parity/upstreams.yaml neovim revision {pin['revision']}). "
            "Refusing: a fixture captured through the wrong binary is not evidence."
        )


def _regtype(rt: str) -> str:
    """Normalize Vim's getregtype() code to a stable observable name."""
    if rt == "":
        return ""  # empty register (nothing yanked/deleted)
    if rt == "v":
        return "charwise"
    if rt == "V":
        return "linewise"
    if rt and rt[0] == "\x16":  # CTRL-V prefix
        return "blockwise"
    return rt


def run_neovim(lines: list[str], keys: str, setup: str = "") -> dict:
    """Run the pinned Neovim on `lines`, feed `keys` in Normal mode, and return the settled state.

    Returns {text, cursor:[row1,col0], reg_unnamed:{text,type}, reg0:{text,type}, mode, nvim_version}.
    The read happens strictly after synchronous feedkeys — see the module docstring for why that is
    the whole point. `setup` is an optional ex command (e.g. `set shiftwidth=4 expandtab`) applied to the
    fresh process before the keys, so a CONFIG-DEPENDENT op can be observed under a config that matches
    ruse's defaults instead of nvim's factory defaults (see the shift_right_line fixture).
    """
    payload = json.dumps({"lines": lines, "keys": keys, "setup": setup})
    src = _LUA % payload
    with tempfile.NamedTemporaryFile(
        "w", suffix=".lua", delete=False, encoding="utf-8"
    ) as fh:
        fh.write(src)
        script = fh.name
    try:
        proc = subprocess.run(
            ["nvim", "--headless", "-u", "NONE", "-i", "NONE", "-l", script],
            capture_output=True,
            text=True,
        )
    finally:
        Path(script).unlink(missing_ok=True)
    if proc.returncode != 0 or not proc.stdout:
        raise OracleError(
            f"nvim failed (rc={proc.returncode}) on keys={keys!r}: "
            f"{proc.stderr.strip() or '<no stderr>'}"
        )
    raw = json.loads(proc.stdout)
    ru_text, ru_type = raw["reg_unnamed"]
    r0_text, r0_type = raw["reg0"]
    return {
        "text": raw["text"],
        "cursor": raw["cursor"],
        "reg_unnamed": {"text": ru_text, "type": _regtype(ru_type)},
        "reg0": {"text": r0_text, "type": _regtype(r0_type)},
        "mode": raw["mode"],
        "nvim_version": nvim_version(),
    }


# --- The fixture corpus: ALREADY-IMPLEMENTED ruse ops. `expect` is captured from the oracle, never
#     hand-written; the (lines, keys) here are the only human-authored part. `<Esc>` is passed
#     verbatim to nvim_replace_termcodes and is understood identically by the Rust key tokenizer.
# A 100-line buffer for the page-scroll fixtures, and the setup that gives nvim ruse's 23-row window
# (an 80x24 terminal with no status line under the text) and ruse's scrolloff.
HUNDRED = [f"line {i}" for i in range(1, 101)]
PAGE_SETUP = "set laststatus=0 scrolloff=3"
# `nvim_buf_set_lines` saves the buffer's first line for `U`; appending a line and deleting it again as a
# whole line drops that, as `dd` does, so a `U` fixture starts with nothing to restore.
U_SETUP = "call append('$', '') | $d _ | call cursor(1, 1)"

FIXTURES: list[dict] = [
    {"name": "x_delete_char", "lines": ["hello"], "keys": "x"},
    {"name": "dw_delete_word", "lines": ["foo bar"], "keys": "dw"},
    {"name": "de_to_word_end", "lines": ["foobar baz"], "keys": "de"},
    {"name": "daw_a_word", "lines": ["foo bar baz"], "keys": "daw"},
    {"name": "diw_inner_word", "lines": ["foo bar baz"], "keys": "diw"},
    {"name": "dd_delete_line", "lines": ["alpha", "beta"], "keys": "dd"},
    {"name": "cw_change_word", "lines": ["foo bar"], "keys": "cwbaz<Esc>"},
    {"name": "yy_p_duplicate_line", "lines": ["one", "two"], "keys": "yyp"},
    {"name": "r_replace_char", "lines": ["hello"], "keys": "rz"},
    {"name": "tilde_toggle_case", "lines": ["abc"], "keys": "~"},
    {"name": "di_paren_inner", "lines": ["foo (bar) baz"], "keys": "di("},
    {"name": "da_quote_around", "lines": ['say "hi" now'], "keys": 'da"'},
    # --- expansion (impl/oracle-harden): already-implemented ruse ops chosen to SURFACE divergence.
    #     operator x motion --------------------------------------------------------------------------
    {"name": "d_to_eol", "lines": ["hello world"], "keys": "d$"},
    {"name": "d_to_bol", "lines": ["hello world"], "keys": "$d0"},
    {"name": "caret_first_non_blank", "lines": ["  foo bar"], "keys": "$^"},
    {"name": "d_caret_from_eol", "lines": ["  foobar"], "keys": "$d^"},
    {"name": "dG_to_last_line", "lines": ["alpha", "beta", "gamma"], "keys": "dG"},
    {"name": "dj_linewise", "lines": ["alpha", "beta", "gamma"], "keys": "dj"},
    {"name": "c_to_eol", "lines": ["hello world"], "keys": "c$END<Esc>"},
    {"name": "y_eol_then_paste", "lines": ["hello"], "keys": "y$p"},
    {"name": "count_dd", "lines": ["alpha", "beta", "gamma"], "keys": "2dd"},
    {"name": "count_x", "lines": ["abcdef"], "keys": "3x"},
    {"name": "d_paragraph_fwd", "lines": ["one", "two", "", "three"], "keys": "d}"},
    #     display-line motions gj/gk/g0/g$/g^ — ruse does NOT soft-wrap (one buffer line == one display
    #     row), so as BARE CURSOR MOTIONS these equal j/k/0/$/^ exactly, as Vim itself does under `nowrap`
    #     (:help gj). Paired with the plain-motion fixtures above, they VERIFY IDENTICALLY. (The OPERATOR
    #     forms `dgj`/`dgk` are NOT fixtured: ruse aliases them to the linewise `dj`/`dk`, whereas nvim
    #     treats `gj`/`gk` as characterwise-exclusive — subject to exclusive-linewise promotion — so nvim's
    #     `dgj` deletes ONE line, not two. A documented deliberate divergence, see input/mod.rs.)
    {"name": "gj_display_down", "lines": ["alpha", "beta"], "keys": "gj"},
    {"name": "gk_display_up", "lines": ["alpha", "beta"], "keys": "jgk"},
    {"name": "g0_display_bol", "lines": ["hello world"], "keys": "$g0"},
    {"name": "gdollar_display_eol", "lines": ["hello world"], "keys": "g$"},
    {"name": "gcaret_display_first_non_blank", "lines": ["  foo bar"], "keys": "$g^"},
    #     `gM` — go to `count`% of the line by char count (bare `gM` = the middle char). The trailing `x`
    #     makes the landing observable (deletes the char under the cursor). ruse counts CHARACTERS where
    #     nvim counts display cells, so these ASCII fixtures agree; tab/wide-char lines are a documented
    #     divergence and deliberately not fixtured.
    {"name": "gM_middle_char", "lines": ["abcdefghij"], "keys": "gMx"},
    {"name": "gM_from_eol", "lines": ["abcdefghij"], "keys": "$gMx"},
    {"name": "gM_count_25_percent", "lines": ["abcdefghij"], "keys": "25gMx"},
    {"name": "gM_count_100_percent", "lines": ["abcdefghij"], "keys": "100gMx"},
    {"name": "gM_odd_length", "lines": ["abcde"], "keys": "gMx"},
    {"name": "dgM_operator_to_middle", "lines": ["abcdefghij"], "keys": "dgM"},
    #     text objects -------------------------------------------------------------------------------
    {"name": "di_bracket_inner", "lines": ["pre[abc]post"], "keys": "f[di["},
    {"name": "da_brace_around", "lines": ["pre{ab}post"], "keys": "f{da{"},
    {"name": "di_dquote_inner", "lines": ['say "hi" now'], "keys": 'di"'},
    {"name": "ci_squote_text", "lines": ["x 'old' y"], "keys": "ci'NEW<Esc>"},
    {"name": "da_angle_around", "lines": ["a<tag>b"], "keys": "f<da<"},
    {"name": "dip_inner_paragraph", "lines": ["one", "two", "", "three"], "keys": "dip"},
    #     actions ------------------------------------------------------------------------------------
    {"name": "J_join_lines", "lines": ["foo", "bar"], "keys": "J"},
    {"name": "count_tilde", "lines": ["abcdef"], "keys": "3~"},
    {"name": "count_r", "lines": ["abcdef"], "keys": "3rz"},
    # CONFIG-DEPENDENT (the only fixture that carries a `setup`): `>>` depends on three editor options.
    # Every OTHER fixture is config-INDEPENDENT — its result is the same under nvim's factory defaults, so
    # it needs no setup and its `expect` is a clean cross-editor claim. `>>` is not: its indent unit and its
    # final cursor both hinge on config, so it is only a valid comparison against ruse when the oracle runs
    # under a config MATCHING ruse's defaults (spec/config-schema.yaml):
    #   editor.indent_style=space  -> `expandtab`      (indent with spaces, not a tab)
    #   editor.tab_width=4         -> `shiftwidth=4`   (one indent level is 4 columns)
    #   ruse homes the cursor to the first non-blank after a shift (Vim's classic `>>`); Neovim's `-u NONE`
    #   default is `nostartofline`, which SUPPRESSES that cursor move, so `startofline` restores it.
    # Under this config the oracle records `"    hello"` with the cursor on the first non-blank — which is
    # exactly what ruse produces from its defaults. (Captured under nvim's own defaults it was a tab.)
    {
        "name": "shift_right_line",
        "lines": ["hello"],
        "keys": ">>",
        "setup": "set shiftwidth=4 expandtab startofline",
    },
    # --- edge-case corpus (impl/corpus-edgecases): CORNERS of already-implemented ops, chosen to
    #     surface subtle correctness bugs the way the earlier di( forward-scan bug was caught.
    #     word motions on punctuation / mixed classes / last word --------------------------------------
    {"name": "dw_on_punct", "lines": ["foo.bar baz"], "keys": "dw"},
    {"name": "de_on_dotted", "lines": ["a.b.c"], "keys": "de"},
    {"name": "dw_last_word", "lines": ["hello world"], "keys": "wdw"},
    #     dw/dW at end-of-line: Vim does NOT let the operator cross the newline (the end of the last word
    #     on the line becomes the end of the operated text), so `dw` on the last word must not join lines.
    {"name": "dw_last_word_joins_next", "lines": ["foo", "bar"], "keys": "dw"},
    {"name": "dw_last_word_trailing_ws", "lines": ["foo   ", "bar"], "keys": "dw"},
    {"name": "dw_midline_last_word_trailing_ws", "lines": ["ab cd  ", "ef"], "keys": "wdw"},
    {"name": "dW_last_word_at_eol", "lines": ["foo.bar", "baz"], "keys": "dW"},
    {"name": "count_dw_crosses_eol", "lines": ["a b", "c d"], "keys": "2dw"},
    #     char-search operators + repeat (f/t/F/T/;/,) ------------------------------------------------
    {"name": "dfx_find_inclusive", "lines": ["abcxdef"], "keys": "dfx"},
    {"name": "dtx_till", "lines": ["abcxdef"], "keys": "dtx"},
    {"name": "dTx_till_back", "lines": ["abcxdef"], "keys": "$dTx"},
    {"name": "dFx_find_back", "lines": ["abcxdef"], "keys": "$dFx"},
    {"name": "semicolon_repeats_find", "lines": ["a.b.c.d"], "keys": "f.;x"},
    {"name": "comma_repeats_find_reverse", "lines": ["a.b.c.d"], "keys": "f.$,x"},
    #     ge/gE backward word-end under operator ------------------------------------------------------
    {"name": "dge_backward_word_end", "lines": ["foo bar"], "keys": "$dge"},
    #     cw special cases (on-blank behaves like cw, not ce) -----------------------------------------
    {"name": "cw_on_blank", "lines": ["foo  bar"], "keys": "llcwZ<Esc>"},
    {"name": "ciw_inner_word", "lines": ["foo bar"], "keys": "ciwZ<Esc>"},
    #     case operators over motion / line ----------------------------------------------------------
    {"name": "gU_word", "lines": ["foo bar"], "keys": "gUw"},
    {"name": "guu_line_lowercase", "lines": ["FOO BAR"], "keys": "guu"},
    {"name": "g_tilde_word", "lines": ["FooBar"], "keys": "g~w"},
    #     line-operator synonyms (D/C/S) -------------------------------------------------------------
    {"name": "cap_D_to_eol", "lines": ["hello world"], "keys": "wD"},
    {"name": "cap_C_change_eol", "lines": ["hello world"], "keys": "wCbye<Esc>"},
    {"name": "cap_S_change_line", "lines": ["hello", "next"], "keys": "Sbye<Esc>"},
    #     % match under operator + inclusive ---------------------------------------------------------
    {"name": "d_percent_paren", "lines": ["a(bcd)e"], "keys": "f(d%"},
    #     dot-repeat -----------------------------------------------------------------------------------
    {"name": "dot_repeats_dw", "lines": ["a b c d"], "keys": "dw."},
    {"name": "dot_repeats_x", "lines": ["abcdef"], "keys": "x.."},
    #     named registers --------------------------------------------------------------------------
    {"name": "named_reg_yank_paste", "lines": ["one", "two"], "keys": '"ayyj"ap'},
    #     inner/around block on its own lines — Vim's "linewise inner block" special case ------------
    {"name": "ci_paren_multiline", "lines": ["foo(", "bar", ")baz"], "keys": "ci(Z<Esc>"},
    {"name": "di_paren_multiline", "lines": ["foo(", "bar", ")baz"], "keys": "di("},
    {"name": "ci_brace_multiline_2lines", "lines": ["fn(){", "a", "b", "}"], "keys": "jci{Z<Esc>"},
    {"name": "di_brace_multiline_2lines", "lines": ["fn(){", "a", "b", "}"], "keys": "jdi{"},
    {"name": "da_paren_multiline", "lines": ["foo(", "bar", ")baz"], "keys": "da("},
    # NOT linewise: content shares the open/close line, so it stays charwise (regression guard).
    {"name": "ci_paren_multiline_inline_open", "lines": ["foo(bar", "baz)qux"], "keys": "ci(Z<Esc>"},
    {"name": "ci_paren_single_line", "lines": ["foo(bar)baz"], "keys": "f(ci(Z<Esc>"},
    {"name": "ci_brace_indented_inner", "lines": ["fn(){", "    body", "}"], "keys": "jci{X<Esc>"},
    #     visual-mode operators ----------------------------------------------------------------------
    {"name": "visual_iw_delete", "lines": ["foo bar"], "keys": "viwd"},
    {"name": "visual_line_delete", "lines": ["a", "b", "c"], "keys": "Vjd"},
    {"name": "visual_block_delete", "lines": ["abc", "def", "ghi"], "keys": "<C-v>jjld"},
    #     operator-forced blockwise CHANGE (`c<C-v>{motion}{text}<Esc>`): delete the block column then
    #     replicate the typed text down every row — same semantics as Visual-block `c`. -----------------
    {"name": "c_forced_block_replicate", "lines": ["abc", "def", "ghi"], "keys": "c<C-v>2jX<Esc>"},
    {"name": "c_forced_block_short_row", "lines": ["abc", "d", "ghi"], "keys": "c<C-v>2jX<Esc>"},
    {"name": "c_forced_block_multichar", "lines": ["abc", "def", "ghi"], "keys": "c<C-v>2jXY<Esc>"},
    {"name": "c_forced_block_wide_1row", "lines": ["abcd", "efgh"], "keys": "c<C-v>2lX<Esc>"},
    #     operators on empty / single-char / last line ------------------------------------------------
    {"name": "x_on_empty_line", "lines": [""], "keys": "x"},
    {"name": "dw_on_empty_line", "lines": [""], "keys": "dw"},
    {"name": "dd_on_last_line", "lines": ["alpha", "beta"], "keys": "Gdd"},
    {"name": "j_past_last_line", "lines": ["alpha", "beta"], "keys": "Gj"},
    #     ROUND 2 sweep — visual/text-object/count/dot-repeat corners --------------------------------
    #     char-search with counts
    {"name": "d2fx_count_find", "lines": ["axbxcxd"], "keys": "d2fx"},
    #     tag + sentence + more text objects
    {"name": "dit_tag_inner", "lines": ["<a>hi</a>"], "keys": "fhdit"},
    {"name": "dat_tag_around", "lines": ["x<b>hi</b>y"], "keys": "fhdat"},
    #     tag objects: nesting, count-expands-outward, attrs, cursor-on-tag, change, multiline ---------
    {"name": "dit_tag_nested_inner", "lines": ["<a><b>x</b></a>"], "keys": "fxdit"},
    {"name": "dat_tag_nested", "lines": ["<a><b>x</b></a>"], "keys": "fxdat"},
    {"name": "dit_tag_count_outer", "lines": ["<a><b>x</b></a>"], "keys": "fx2dit"},
    {"name": "dat_tag_count_outer", "lines": ["<a><b>x</b></a>"], "keys": "fx2dat"},
    {"name": "cit_tag_inner", "lines": ["<div>hello</div>"], "keys": "fhcitZ<Esc>"},
    {"name": "dat_tag_attrs", "lines": ['<a href="x">hi</a>'], "keys": "fhdat"},
    {"name": "dit_tag_on_open", "lines": ["<div>hello</div>"], "keys": "ldit"},
    {"name": "dit_tag_multiline", "lines": ["<div>", "hello", "</div>"], "keys": "jdit"},
    {"name": "yat_tag_multiline", "lines": ["<div>", "hello", "</div>"], "keys": "jyat"},
    # `dat` on a whole-line block is the ONE documented tag divergence: nvim DELETES linewise here
    # (regtype V), while ruse — one span for d/y/c, matching nvim's own charwise `yat`/`cat` — is
    # charwise. The buffer text is identical; only the register TYPE differs. Kept as an honest finding.
    {"name": "dat_tag_multiline", "lines": ["<div>", "hello", "</div>"], "keys": "jdat"},
    {"name": "vit_tag_delete", "lines": ["<div>hello</div>"], "keys": "fhvitd"},
    {"name": "vat_tag_delete", "lines": ["<div>hello</div>"], "keys": "fhvatd"},
    {"name": "das_sentence_around", "lines": ["One. Two. Three."], "keys": "wdas"},
    {"name": "dis_sentence_inner", "lines": ["One. Two. Three."], "keys": "wdis"},
    {"name": "ci_angle_inner", "lines": ["a<bcd>e"], "keys": "f<ci<Z<Esc>"},
    #     visual-mode operators
    {"name": "v_iw_change", "lines": ["foo bar"], "keys": "viwcZ<Esc>"},
    {"name": "v_aw_delete", "lines": ["foo bar baz"], "keys": "wvawd"},
    {"name": "V_line_yank_paste", "lines": ["one", "two"], "keys": "Vyp"},
    {"name": "v_gU_upcase", "lines": ["foo bar"], "keys": "vllgU"},
    {"name": "v_tilde_toggle", "lines": ["FooBar"], "keys": "vlll~"},
    {"name": "v_ip_paragraph", "lines": ["a", "b", "", "c"], "keys": "vipd"},
    {"name": "v_swap_ends_o", "lines": ["hello"], "keys": "lvllohd"},
    #     blockwise insert / append
    {"name": "block_insert_I", "lines": ["ab", "cd", "ef"], "keys": "<C-v>jjIX<Esc>"},
    {"name": "block_append_A", "lines": ["ab", "cd", "ef"], "keys": "<C-v>jj$AX<Esc>"},
    #     dot-repeat corners
    {"name": "dot_repeats_ciw", "lines": ["foo bar baz"], "keys": "ciwX<Esc>w."},
    {"name": "dot_repeats_A_append", "lines": ["a", "b"], "keys": "A!<Esc>j."},
    #     paste geometry corners
    {"name": "gp_moves_after", "lines": ["one", "two"], "keys": "yygpx"},
    {"name": "numbered_reg_after_dd", "lines": ["a", "b", "c"], "keys": "ddjdd\"1p"},
    #     ROUND 3 sweep — counts, marks, line-case, idioms ------------------------------------------
    #     operator to a named mark (charwise ` / linewise ')
    {"name": "d_backtick_mark_charwise", "lines": ["abc def ghi"], "keys": "wmawd`a"},
    {"name": "d_quote_mark_linewise", "lines": ["one", "two", "three", "four"], "keys": "majjd'a"},
    {"name": "c_backtick_mark_change", "lines": ["abc def ghi"], "keys": "wmawc`aZ<Esc>"},
    #     case/shift/reindent operators to a named mark (this change) — set mark `a`, move, operate.
    #     case: charwise ` honours the [cursor,mark) span; linewise ' spans whole lines.
    {"name": "gU_backtick_mark_charwise", "lines": ["abc def ghi"], "keys": "wmawgU`a"},
    {"name": "gu_backtick_mark_down", "lines": ["ABC DEF GHI"], "keys": "wwmabgu`a"},
    {"name": "g_tilde_backtick_mark", "lines": ["aBcDeF"], "keys": "mallg~`a"},
    {"name": "gU_quote_mark_linewise", "lines": ["one two", "three four", "five six"], "keys": "majjgU'a"},
    {"name": "gu_quote_mark_linewise", "lines": ["ONE", "TWO", "THREE"], "keys": "majjgu'a"},
    #     shift: ALWAYS linewise regardless of `/' — spans cursor line .. mark line. Config-dependent.
    {"name": "shift_right_backtick_mark", "lines": ["a", "b", "c"], "keys": "majj>`a", "setup": "set shiftwidth=4 expandtab startofline"},
    {"name": "shift_right_quote_mark", "lines": ["a", "b", "c"], "keys": "majj>'a", "setup": "set shiftwidth=4 expandtab startofline"},
    {"name": "shift_left_backtick_mark", "lines": ["    a", "    b", "    c"], "keys": "majj<`a", "setup": "set shiftwidth=4 expandtab startofline"},
    {"name": "shift_right_mark_above", "lines": ["a", "b", "c"], "keys": "jjmakk>`a", "setup": "set shiftwidth=4 expandtab startofline"},
    #     NOTE: `=`a` (reindent to a mark) is NOT fixtured here — ruse's `=` is a bracket-depth reindent,
    #     while nvim's `=` under `-u NONE` (equalprg/indentexpr empty, nocindent) is plain autoindent, so
    #     they legitimately diverge (the same reason no `=`-motion fixture exists). Verified by a core unit
    #     test (reindent_to_mark) against ruse's own `=`-over-motion semantics instead.
    #     counted text objects
    #     counted actions
    {"name": "count_3J_join", "lines": ["a", "b", "c", "d"], "keys": "3J"},
    {"name": "count_2cc_change", "lines": ["one", "two", "three"], "keys": "2ccX<Esc>"},
    #     operator to a mark (linewise ' vs charwise `)
    #     line-case operators
    {"name": "gUU_line_upcase", "lines": ["foo bar"], "keys": "gUU"},
    {"name": "g_tilde_tilde_line", "lines": ["FooBar"], "keys": "g~~"},
    #     quote-object corners
    {"name": "ci_dquote_empty", "lines": ['say "" now'], "keys": 'f"ci"Z<Esc>'},
    {"name": "di_dquote_before", "lines": ['x "hi" y'], "keys": 'di"'},
    #     classic idioms
    {"name": "ddp_swap_lines", "lines": ["one", "two"], "keys": "ddp"},
    {"name": "xp_transpose_chars", "lines": ["ab"], "keys": "xp"},
    #     percentage jump + count %
    #     counted text objects
    {"name": "d2aw_count_object", "lines": ["foo bar baz qux"], "keys": "d2aw"},
    {"name": "count_2daw", "lines": ["foo bar baz qux"], "keys": "2daw"},
    {"name": "d2iw_count_inner", "lines": ["foo bar baz"], "keys": "d2iw"},
    {"name": "d3iw_count_inner", "lines": ["foo bar baz"], "keys": "d3iw"},
    {"name": "d2aW_count_big", "lines": ["foo.bar baz qux"], "keys": "d2aW"},
    {"name": "c2iw_count_change", "lines": ["foo bar baz"], "keys": "c2iwZ<Esc>"},
    #     {count}% percentage jump
    {"name": "percent_jump_50", "lines": ["a", "b", "c", "d"], "keys": "50%x"},
    {"name": "percent_jump_100", "lines": ["a", "b", "c", "d"], "keys": "100%x"},
    {"name": "percent_jump_25", "lines": ["a", "b", "c", "d"], "keys": "25%x"},
    {"name": "d_percent_jump_linewise", "lines": ["a", "b", "c", "d"], "keys": "d50%"},
    #     bare w/W at end-of-line: no next word -> rest ON the last char (Normal can't rest past it)
    {"name": "w_last_word_rests_on_last_char", "lines": ["abc def"], "keys": "wwx"},
    {"name": "w_single_word_rests_on_last_char", "lines": ["abc"], "keys": "wx"},
    {"name": "bigw_last_word_rests_on_last_char", "lines": ["foo.bar baz"], "keys": "WWx"},
    #     ROUND 4 sweep — replace mode / case-over-object / search-motion op / sentences / indent ------
    #     Replace mode (R)
    {"name": "replace_mode_overwrites", "lines": ["abcdef"], "keys": "RXYZ<Esc>"},
    {"name": "replace_mode_past_eol_appends", "lines": ["ab"], "keys": "RXYZ<Esc>"},
    {"name": "replace_mode_backspace_restores", "lines": ["abcdef"], "keys": "RXY<BS><BS><Esc>"},
    #     case operators over a text object
    {"name": "gU_inner_word", "lines": ["foo bar"], "keys": "gUiw"},
    {"name": "gu_a_word", "lines": ["FOO BAR"], "keys": "guaw"},
    {"name": "g_tilde_inner_word", "lines": ["FooBar baz"], "keys": "g~iw"},
    {"name": "gU_a_paragraph", "lines": ["one two", "three"], "keys": "gUip"},
    #     case operators over a motion
    {"name": "gU_to_eol", "lines": ["foo bar"], "keys": "gU$"},
    {"name": "gu_word_motion", "lines": ["FOO BAR"], "keys": "guw"},
    #     sentence motions
    {"name": "d_sentence_fwd", "lines": ["One two. Three four."], "keys": "d)"},
    {"name": "sentence_back", "lines": ["One. Two three."], "keys": "$(x"},
    #     paragraph object around
    {"name": "dap_around_paragraph", "lines": ["a", "b", "", "c"], "keys": "dap"},
    #     gi resume insert
    {"name": "gi_resume_insert", "lines": ["abc"], "keys": "iX<Esc>llgiY<Esc>"},
    #     increment edge cases
    {"name": "increment_hex_ff", "lines": ["0xff"], "keys": "<C-a>"},
    {"name": "increment_negative_wraps", "lines": ["-1"], "keys": "<C-a>"},
    {"name": "increment_binary", "lines": ["0b101"], "keys": "<C-a>"},
    #     Visual CTRL-A / g CTRL-A — per-line and successive (sequence) increment over a selection
    {"name": "visual_increment_column", "lines": ["0", "0", "0"], "keys": "VG<C-a>"},
    {"name": "visual_seq_increment_run", "lines": ["0", "0", "0"], "keys": "VGg<C-a>"},
    {"name": "visual_seq_increment_count", "lines": ["0", "0", "0"], "keys": "VG2g<C-a>"},
    {"name": "visual_seq_skip_blank", "lines": ["0", "", "0", "0"], "keys": "VGg<C-a>"},
    {"name": "visual_seq_skip_nonumber", "lines": ["0", "abc", "0"], "keys": "VGg<C-a>"},
    {"name": "visual_increment_leading_zero", "lines": ["007", "008"], "keys": "VG<C-a>"},
    {"name": "visual_seq_negatives", "lines": ["-3", "-3"], "keys": "VGg<C-a>"},
    {"name": "visual_seq_decrement", "lines": ["5", "5", "5"], "keys": "VGg<C-x>"},
    {"name": "visual_seq_first_line_no_number", "lines": ["abc", "5", "5"], "keys": "VGg<C-a>"},
    {"name": "block_seq_increment", "lines": ["x0", "x0", "x0"], "keys": "l<C-v>jjg<C-a>"},
    {"name": "block_increment_covers_number", "lines": ["07", "08"], "keys": "<C-v>jl<C-a>"},
    #     replace char across multibyte
    {"name": "replace_char_unicode", "lines": ["αβγ"], "keys": "lrx"},
    #     ROUND 4b — search-motion operators / blockwise / object aliases / register append
    {"name": "block_change_column", "lines": ["abc", "def", "ghi"], "keys": "\x16jjcX\x1b"},
    {"name": "block_yank_paste", "lines": ["abc", "def"], "keys": "\x16jly$p"},
    {"name": "dib_paren_alias", "lines": ["x(abc)y"], "keys": "fadib"},
    {"name": "dab_brace_alias", "lines": ["x{abc}y"], "keys": "fadaB"},
    {"name": "cip_inner_paragraph", "lines": ["one", "two", "", "three"], "keys": "cipZ<Esc>"},
    {"name": "register_append_uppercase", "lines": ["one", "two"], "keys": "\"ayyj\"Ayyk\"ap"},
    #     operator + search motion (d/c/y followed by /pat<CR>) — already implemented; confirm parity
    {"name": "d_search_motion", "lines": ["foo bar baz"], "keys": "d/baz<CR>"},
    {"name": "y_search_then_paste", "lines": ["foo bar baz"], "keys": "y/bar<CR>P"},
    {"name": "c_search_motion", "lines": ["foo bar baz"], "keys": "c/baz<CR>Z<Esc>"},
    #     blockwise $A on ragged lines
    {"name": "block_append_ragged", "lines": ["a", "abc", "ab"], "keys": "<C-v>jj$AX<Esc>"},
    {"name": "block_change_column", "lines": ["abc", "def", "ghi"], "keys": "<C-v>jjcX<Esc>"},
    #     ROUND 6 — indent/shift/format (config-dependent), replace-with-newline, quote/tag corners
    {"name": "shift_right_motion_2lines", "lines": ["a", "b", "c"], "keys": ">j", "setup": "set shiftwidth=4 expandtab startofline"},
    {"name": "shift_left_dedents", "lines": ["    ab"], "keys": "<<", "setup": "set shiftwidth=4 expandtab startofline"},
    {"name": "shift_right_paragraph", "lines": ["a", "b", "", "c"], "keys": ">ip", "setup": "set shiftwidth=4 expandtab startofline"},
    {"name": "gq_reflow_paragraph_79", "lines": ["word word word word word word word word word word word word word word word word word word word word"], "keys": "gqip"},
    #     replace char with a newline (splits the line)
    {"name": "replace_char_with_newline", "lines": ["abcdef"], "keys": "llr<CR>"},
    #     quote / tag object corners
    {"name": "ci_squote_adjacent", "lines": ["'a' 'b'"], "keys": "ci'Z<Esc>"},
    {"name": "cit_tag_inner", "lines": ["<p>hi</p>"], "keys": "fhcitZ<Esc>"},
    {"name": "di_backtick_quote", "lines": ["x `code` y"], "keys": "fcdi`"},
    {"name": "da_squote_around", "lines": ["say 'hi' ok"], "keys": "fhda'"},
    #     daw on trailing punctuation
    {"name": "daw_before_punct", "lines": ["foo, bar"], "keys": "daw"},
    #     multi-byte / unicode buffers (byte-index correctness) --------------------------------------
    {"name": "x_unicode_multibyte", "lines": ["αβγ"], "keys": "x"},
    {"name": "x_emoji", "lines": ["a😀b"], "keys": "lx"},
    {"name": "dw_unicode", "lines": ["héllo wörld"], "keys": "dw"},
    {"name": "tilde_unicode", "lines": ["αβ"], "keys": "~"},
    {"name": "di_dquote_unicode", "lines": ['say "héllo" now'], "keys": 'di"'},
    #     delimiter nesting ---------------------------------------------------------------------------
    {"name": "di_paren_nested_outer", "lines": ["(a(b)c)"], "keys": "di("},
    {"name": "di_paren_nested_inner", "lines": ["(a(b)c)"], "keys": "f(di("},
    {"name": "da_paren_innermost", "lines": ["(a(b)c)"], "keys": "fbda("},
    #     visual mode ---------------------------------------------------------------------------------
    {"name": "viwd_visual_inner_word", "lines": ["foo bar baz"], "keys": "wviwd"},
    {"name": "Vd_visual_line", "lines": ["alpha", "beta"], "keys": "Vd"},
    {"name": "vjd_visual_charwise_lines", "lines": ["hello", "world"], "keys": "vjd"},
    {"name": "v_dollar_y", "lines": ["hello"], "keys": "v$y"},
    #     paste geometry ------------------------------------------------------------------------------
    {"name": "yy_p_last_line", "lines": ["one", "two"], "keys": "Gyyp"},
    {"name": "x_then_p", "lines": ["abc"], "keys": "xp"},
    #     count edges ---------------------------------------------------------------------------------
    {"name": "count_5x_past_eol", "lines": ["abc"], "keys": "5x"},
    {"name": "count_3dd_past_last", "lines": ["alpha", "beta"], "keys": "3dd"},
    # --- op-family expansion (impl/corpus-visual-paste): new op FAMILIES chosen to surface parity
    #     divergences. Edge cases of IMPLEMENTED ops dominate; a single probe documents each op ruse
    #     does not implement yet. Every `expect` is still oracle-captured, never hand-written.
    #     VISUAL editing (edge cases of the implemented v/V + selection operators) -------------------
    {"name": "Vjd_visual_linewise", "lines": ["alpha", "beta", "gamma"], "keys": "Vjd"},
    # visual-LINEWISE change (`V…c`) must behave like `cc` over the range: replace the whole selected
    # lines with ONE empty line, KEEP the separator to the following line (never merge the next line in),
    # and enter Insert. Regression net for the "collapses the line separator" fix (issue #435), paired
    # with the `cc`/`2cc` linewise-change and `vjc` charwise-change probes below.
    {"name": "Vjc_visual_linewise_change", "lines": ["abc", "beta", "gamma"], "keys": "VjcX<Esc>"},
    {"name": "Vc_visual_linewise_change_one", "lines": ["abc", "beta", "gamma"], "keys": "VcX<Esc>"},
    {"name": "VGc_visual_linewise_change_all", "lines": ["abc", "beta", "gamma"], "keys": "VGcX<Esc>"},
    {"name": "Vjjc_visual_linewise_change_three", "lines": ["abc", "beta", "gamma"], "keys": "VjjcX<Esc>"},
    {"name": "cc_linewise_change", "lines": ["abc", "beta", "gamma"], "keys": "ccX<Esc>"},
    {"name": "count_cc_linewise_change", "lines": ["abc", "beta", "gamma"], "keys": "2ccX<Esc>"},
    {"name": "vjc_charwise_change_no_regress", "lines": ["abc", "beta", "gamma"], "keys": "vjcX<Esc>"},
    {"name": "Vjd_delete_no_regress", "lines": ["abc", "beta", "gamma"], "keys": "Vjd"},
    {"name": "vey_visual_yank_to_word_end", "lines": ["hello world"], "keys": "vey"},
    {"name": "v_dollar_d_visual", "lines": ["hello world"], "keys": "v$d"},
    {"name": "viwc_visual_change_text", "lines": ["foo bar baz"], "keys": "wviwcNEW<Esc>"},
    #     VISUAL probes (ops ruse does not implement: `o` swap-ends, `gv` reselect) ------------------
    {"name": "visual_o_swap_then_extend", "lines": ["abcde"], "keys": "lllvhold"},
    {"name": "gv_reselect", "lines": ["hello world"], "keys": "viwygvd"},
    #     PASTE geometry (edge cases of the implemented p/P) -----------------------------------------
    {"name": "yl_p_charwise_after", "lines": ["abc"], "keys": "ylp"},
    {"name": "yl_P_charwise_before", "lines": ["abc"], "keys": "ylP"},
    {"name": "yy_P_linewise_above", "lines": ["one", "two"], "keys": "yyP"},
    {"name": "charwise_paste_at_eol", "lines": ["abc"], "keys": "yl$p"},
    {"name": "dd_p_linewise_put", "lines": ["one", "two", "three"], "keys": "ddp"},
    {"name": "count_2p_charwise", "lines": ["abc"], "keys": "yl2p"},
    #     CHAR-SEARCH operators (edge cases of the implemented f/F/t/T + ; under an operator) --------
    {"name": "dtx_till_forward", "lines": ["abcxdef"], "keys": "dtx"},
    {"name": "dfx_find_forward", "lines": ["abcxdef"], "keys": "dfx"},
    {"name": "ctx_change_text", "lines": ["abcxdef"], "keys": "ctxYZ<Esc>"},
    {"name": "dTx_till_backward", "lines": ["xabcd"], "keys": "$dTx"},
    {"name": "d_semicolon_repeat_under_op", "lines": ["a.b.c.d"], "keys": "f.d;"},
    #     LINE-operator synonyms — mostly probes for ops ruse does not implement (D/C/Y), plus `cc` ---
    {"name": "cc_change_line", "lines": ["  hello", "world"], "keys": "ccNEW<Esc>"},
    {"name": "D_delete_to_eol", "lines": ["hello world"], "keys": "wD"},
    {"name": "C_change_to_eol", "lines": ["hello world"], "keys": "wCX<Esc>"},
    {"name": "Y_yank_line", "lines": ["hello", "world"], "keys": "Yp"},
    {"name": "S_change_line", "lines": ["  hello", "world"], "keys": "SNEW<Esc>"},
    # --- COMPOSITE corpus (impl/corpus-composite): multi-step editing SESSIONS (6-20 keystrokes) whose
    #     whole point is CROSS-COMMAND STATE — one command's effect (dot-register, unnamed register,
    #     cursor, mode) carried into the next. The atomic (1-2 op) fixtures above cannot surface an
    #     INTERACTION bug; these can. Every `expect` is still oracle-captured, never hand-written.
    #     DOT-REPEAT chains (`.` replays the last change; verify dd/insert repeatability via the oracle) --
    {"name": "dot_ciw_repeat_word", "lines": ["foo bar baz"], "keys": "ciwX<Esc>w."},
    {"name": "dot_x_across_lines", "lines": ["abc", "def"], "keys": "xj."},
    {"name": "dot_dd_repeat", "lines": ["a", "b", "c"], "keys": "dd."},
    {"name": "dot_A_append_repeat", "lines": ["foo", "bar"], "keys": "A;<Esc>j."},
    {
        "name": "dot_shift_repeat",
        "lines": ["a", "b"],
        "keys": ">>j.",
        "setup": "set shiftwidth=4 expandtab startofline",
    },
    {"name": "dot_dw_twice", "lines": ["one two three four"], "keys": "dw.."},
    #     COUNT accumulation interacting with a later command / dot --------------------------------------
    {"name": "count_d2w_then_dot", "lines": ["a b c d e"], "keys": "d2w."},
    {"name": "count_3x_then_dot", "lines": ["abcdefgh"], "keys": "3x."},
    {"name": "count_2dd_then_p", "lines": ["a", "b", "c", "d"], "keys": "2ddp"},
    #     REGISTER reuse across ops (unnamed register survives motions; `"a` is a NAMED-register probe) ---
    {"name": "reg_dw_word_paste_before", "lines": ["one two three"], "keys": "dwwP"},
    {"name": "reg_dd_overwrite_then_paste", "lines": ["1", "2", "3", "4"], "keys": "ddjddp"},
    {"name": "reg_named_yank_paste", "lines": ["foo bar"], "keys": '"ayiw$"ap'},
    {"name": "reg_named_yy_paste", "lines": ["one", "two"], "keys": '"ayy"ap'},
    {"name": "reg_named_dd_paste", "lines": ["one", "two"], "keys": '"add"ap'},
    {"name": "reg_append_uppercase", "lines": ["foo", "bar"], "keys": '"Ayiwj"Ayiw"ap'},
    #     YANK REGISTER "0 (quote0): last yank, untouched by deletes; a NAMED yank does not set it ----------
    {"name": "reg0_yank_then_paste", "lines": ["ab cd"], "keys": 'yiw$"0p'},
    {"name": "reg0_survives_delete", "lines": ["foo bar"], "keys": 'yiwwdiw"0p'},
    {"name": "reg0_named_yank_leaves_it_empty", "lines": ["foo bar"], "keys": '"ayiww"0p'},
    #     MODE-TRANSITION sequences (selection/insert -> normal -> paste; append then insert-at-start) ----
    {"name": "mode_viwy_open_paste", "lines": ["hello"], "keys": "viwyo<Esc>p"},
    {"name": "mode_vjd_then_paste", "lines": ["abc", "def"], "keys": "vjdp"},
    {"name": "mode_append_then_insert_bol", "lines": ["mid"], "keys": "A!<Esc>0i?<Esc>"},
    #     FORCED MOTION WISE (o_v / o_V / o_CTRL-V): force charwise on a linewise motion, force linewise on
    #     a charwise motion, toggle a charwise motion's exclusive/inclusive edge, and force blockwise. ------
    {"name": "forced_dvj_charwise", "lines": ["hello", "world"], "keys": "dvj"},
    {"name": "forced_dvk_charwise", "lines": ["hello", "world"], "keys": "jdvk"},
    {"name": "forced_dVe_linewise", "lines": ["hello world"], "keys": "dVe"},
    {"name": "forced_dVw_linewise", "lines": ["hello world"], "keys": "dVw"},
    {"name": "forced_dvw_toggle_inclusive", "lines": ["hello world"], "keys": "dvw"},
    {"name": "forced_dve_toggle_exclusive", "lines": ["hello world"], "keys": "dve"},
    {"name": "forced_dv_dollar_toggle", "lines": ["hello world"], "keys": "dv$"},
    {"name": "forced_dV_paragraph", "lines": ["one", "two", "", "three"], "keys": "dV}"},
    {"name": "forced_yvj_then_paste", "lines": ["hello", "world"], "keys": "yvjP"},
    #     BLOCKWISE VISUAL (CTRL-V) — F-025/F-029/F-023 slice 1: a column-aligned rectangle. Yank/delete
    #     capture per-row slices into a blockwise register; `p`/`P` drop the block back as a rectangle; the
    #     operator-pending `CTRL-V` forces a blockwise operator (`d<C-v>j`). Byte/char columns (ASCII here).
    {"name": "block_delete_2x2", "lines": ["abc", "def", "ghi"], "keys": "<C-v>ljd"},
    {"name": "block_delete_1col_3rows", "lines": ["abc", "def", "ghi"], "keys": "<C-v>jjd"},
    {"name": "block_yank_paste_before", "lines": ["abc", "def", "ghi"], "keys": "<C-v>ljyP"},
    {"name": "block_yank_paste_after_1col", "lines": ["abc", "def"], "keys": "<C-v>jyp"},
    {"name": "forced_d_ctrl_v_j_blockwise", "lines": ["abc", "def"], "keys": "d<C-v>j"},
    #     BLOCKWISE INSERT-REPLICATE (slice 2): `I`/`A`/`c` type on the top row and replicate the typed text
    #     down every block row on <Esc> (`A` pads short lines; `c` deletes the block first). ----------------
    {"name": "block_insert_I_replicate", "lines": ["abc", "def", "ghi"], "keys": "<C-v>jjIX<Esc>"},
    {"name": "block_append_A_replicate", "lines": ["abc", "def"], "keys": "<C-v>jAX<Esc>"},
    {"name": "block_change_c_replicate", "lines": ["abc", "def"], "keys": "<C-v>jcX<Esc>"},
    {"name": "block_append_A_pads_short", "lines": ["ab", "c"], "keys": "<C-v>ljAX<Esc>"},
    #     BLOCKWISE INSERT DOT-REPEAT: `.` rebuilds a block of the SAME width x height at the caret and
    #     re-applies the same I/A/c with the same typed text (nvim reuses the geometry, positioned at the
    #     cursor). Count on `.` is ignored for block insert; rows clamp at end-of-buffer. -------------------
    {"name": "block_I_dot", "lines": ["aaaa", "bbbb", "cccc", "dddd"], "keys": "<C-v>jIX<Esc>jj."},
    {"name": "block_A_dot", "lines": ["aaaa", "bbbb", "cccc", "dddd"], "keys": "<C-v>jllAX<Esc>jj."},
    {"name": "block_c_dot", "lines": ["aaaa", "bbbb", "cccc", "dddd"], "keys": "<C-v>jlcXY<Esc>jj."},
    {"name": "block_I_dot_at_col", "lines": ["aaaaaa", "bbbbbb", "cccccc", "dddddd"], "keys": "<C-v>jIX<Esc>jjll."},
    {"name": "block_c_dot_at_col", "lines": ["aaaaaa", "bbbbbb", "cccccc", "dddddd", "eeeeee"], "keys": "<C-v>jllcZ<Esc>jjj."},
    {"name": "block_A_dot_moved", "lines": ["aaaaaa", "bbbbbb", "cccccc", "dddddd"], "keys": "<C-v>jlAQ<Esc>jjl."},
    {"name": "block_I_dot_count_ignored", "lines": ["aaaa", "bbbb", "cccc"], "keys": "<C-v>IX<Esc>j2."},
    {"name": "block_I_dot_eof_clamp", "lines": ["aaaa", "bbbb", "cccc", "dddd"], "keys": "<C-v>jjIZ<Esc>Gk."},
    {"name": "block_A_dollar_dot", "lines": ["aa", "bbbb", "c", "dddd"], "keys": "<C-v>j$AZ<Esc>jj."},
    #     CURSWANT — the sticky desired column. `j`/`k` keep the wanted column across a SHORT interior line
    #     instead of collapsing to its end; `$` sets curswant to MAXCOL so `j`/`k` ride each line's last char.
    # Blockwise over a short interior line: curswant keeps the block full-width (was a probe; now verified).
    {"name": "block_ragged_short_line", "lines": ["abcd", "x", "efgh"], "keys": "l<C-v>lljjd"},
    # Plain `j` through a short line restores the wanted column on the next long line ('c' col2 -> 'n' col2).
    {"name": "curswant_j_through_short_line", "lines": ["abc", "x", "lmnop"], "keys": "lljjrY"},
    # `$` then `j`: the cursor rides each line's END (last char), not a fixed column.
    {"name": "curswant_dollar_rides_ends", "lines": ["ab", "wxyz", "c"], "keys": "$jrY"},
    #     REPLACE MODE (R): overwrite policy + <BS> restores the overwritten char / deletes an appended one.
    {"name": "replace_basic_overwrite", "lines": ["hello"], "keys": "Rxy<Esc>"},
    {"name": "replace_past_eol_appends", "lines": ["ab"], "keys": "RXYZ<Esc>"},
    {"name": "replace_backspace_restores", "lines": ["hello"], "keys": "Rxy<BS><BS><Esc>"},
    {"name": "replace_backspace_deletes_appended", "lines": ["ab"], "keys": "RXYZ<BS><Esc>"},
    #     VIRTUAL REPLACE (gR): tab-aware overwrite. Without tabs it is identical to R. Over a <Tab> the
    #     typed char is inserted before the tab (shrinking it) until its last virtual column, then replaces
    #     it — preserving the on-screen layout. `set tabstop=4` matches ruse's editor.tab_width default so
    #     the virtual-column arithmetic aligns; `noexpandtab` keeps a literal <Tab> in the buffer.
    {"name": "gr_basic_overwrite", "lines": ["hello"], "keys": "gRxy<Esc>"},
    {"name": "gr_past_eol_appends", "lines": ["ab"], "keys": "gRXYZ<Esc>"},
    {"name": "gr_backspace_restores", "lines": ["hello"], "keys": "gRxy<BS><BS><Esc>"},
    {"name": "gr_over_tab_shrinks", "lines": ["a\tb"], "keys": "lgRXY<Esc>", "setup": "set tabstop=4 noexpandtab"},
    {"name": "gr_consumes_tab_fully", "lines": ["a\tb"], "keys": "lgRXYZ<Esc>", "setup": "set tabstop=4 noexpandtab"},
    {"name": "gr_over_tab_then_overwrite", "lines": ["a\tb"], "keys": "lgRXYZW<Esc>", "setup": "set tabstop=4 noexpandtab"},
    {"name": "gr_over_tab_backspace_regrows", "lines": ["a\tb"], "keys": "lgRX<BS><Esc>", "setup": "set tabstop=4 noexpandtab"},
    #     `gI` — insert at column 1 (byte col 0), BEFORE all indentation (unlike `I` = first non-blank).
    #     `{count}gI` repeats the typed text like the other insert-entries. `<Tab>` here is layout only; the
    #     inserted text is plain chars, so the result is tabstop-independent (no setup needed).
    {"name": "gI_insert_column_zero", "lines": ["\thello"], "keys": "gIX<Esc>", "setup": "set noexpandtab"},
    {"name": "gI_before_space_indent", "lines": ["  ab"], "keys": "gIX<Esc>"},
    {"name": "gI_count_repeats_text", "lines": ["\thello"], "keys": "3gIx<Esc>", "setup": "set noexpandtab"},
    #     `gr{char}` — CLASSIC-Vim virtual-replace of ONE (or `{count}`) char, then back to Normal (the one-
    #     shot of `gR`, as `r` is to `R`). NOTE: nvim 0.11+ maps `gr`/`grn`/`gra`/`grr` to LSP by default, but
    #     under `-u NONE` those maps are absent, so bare `gr{char}` here is the classic built-in. Tab-aware:
    #     over a multi-column <Tab> the char inserts before it, preserving the following column.
    {"name": "grchar_one_char", "lines": ["abcdef"], "keys": "lgrX"},
    {"name": "grchar_count", "lines": ["abcdef"], "keys": "3grZ"},
    {"name": "grchar_past_eol_appends", "lines": ["ab"], "keys": "4grX"},
    {"name": "grchar_over_tab", "lines": ["\tX"], "keys": "grA", "setup": "set tabstop=4 noexpandtab"},
    {"name": "grchar_count_over_tab", "lines": ["\tX"], "keys": "2grA", "setup": "set tabstop=4 noexpandtab"},
    # (No `R`-then-`u` fixture: the oracle sets the buffer via set_lines, which is NOT an undo boundary, so
    #  `u` undoes past the initial content to empty — an oracle artifact, not Vim behavior. R+undo is covered
    #  by a core unit test instead.)
    #     INSERT `CTRL-O` (i_CTRL-O): run ONE Normal command from Insert, then return to Insert. Observable
    #     via the resulting text+cursor. (CTRL-G u — undo-break — is NOT observable here for the same
    #     set_lines-is-not-an-undo-boundary reason as R+undo, so it is a core unit test, not a fixture.)
    #     ruse implements the one-shot by routing the next key(s) through the Normal grammar while the core
    #     stays in Insert (every Normal cmd inherits st.mode; the Normal-only cursor clamp is gated off).
    {"name": "ctrl_o_x_delete", "lines": ["hello"], "keys": "i<C-o>xEND<Esc>"},
    {"name": "ctrl_o_word_motion", "lines": ["foo bar"], "keys": "i<C-o>wX<Esc>"},
    # i_CTRL-O's EOL APPEND-COLUMN (curswant): after `$` (or an `A` whose append intent survives a `dd`),
    # the return-to-insert caret rests at end-of-line so the next char appends. curswant == MAXCOL parks the
    # Insert caret at the append position; a one-shot edit like `dd` preserves that intent (was a probe pair;
    # now verified). Note: `A<C-o>h` (append then a horizontal move) needs the append->last-char pull-back on
    # entering the one-shot, which is a separate refinement — not fixtured here.
    {"name": "ctrl_o_dollar_append", "lines": ["hi"], "keys": "i<C-o>$X<Esc>"},
    {"name": "ctrl_o_dd_delete_line", "lines": ["alpha", "beta"], "keys": "A<C-o>ddX<Esc>"},
    {"name": "ctrl_o_j_rides_to_append", "lines": ["ab", "cd"], "keys": "A<C-o>jX<Esc>"},
    {"name": "ctrl_o_zero_resets_append", "lines": ["alpha", "beta"], "keys": "A<C-o>0X<Esc>"},
    #     DOT-REPEAT around i_CTRL-O (#795): CTRL-O ENDS the insert for redo (Vim's ins_esc), the one-shot
    #     command is classified as a Normal command, and text typed after the return starts a FRESH `i`
    #     change (Vim's stop_arrow resets the redo buffer to `1i`); the entry count is dropped.
    {"name": "dot_ctrl_o_motion_then_text", "lines": ["x", "y"], "keys": "iab<C-o>0cd<Esc>j."},
    # (`iab<C-o>ucd<Esc>j.` is pinned in apps/tui/tests/dot_repeat.rs: set_lines is not an undo boundary.)
    {"name": "dot_ctrl_o_count_dropped", "lines": ["x"], "keys": "3iab<C-o>hcd<Esc>"},
    {"name": "dot_ctrl_o_no_text_keeps_insert", "lines": ["x", "y"], "keys": "iab<C-o>0<Esc>j."},
    {"name": "dot_ctrl_o_append_then_text", "lines": ["abc", "def"], "keys": "Aab<C-o>0cd<Esc>j."},
    {"name": "dot_ctrl_o_x_then_text", "lines": ["abc", "def"], "keys": "iab<C-o>xcd<Esc>j."},
    {"name": "dot_ctrl_o_x_is_last_change", "lines": ["abc", "def"], "keys": "iab<C-o>x<Esc>j."},
    {"name": "dot_ctrl_o_change_kept", "lines": ["abc", "def"], "keys": "ciwZ<C-o>0<Esc>j."},
    {"name": "dot_ctrl_o_twice", "lines": ["abc", "def"], "keys": "iab<C-o>0cd<C-o>0ef<Esc>j0."},
    {"name": "dot_ctrl_o_count_on_dot", "lines": ["abc", "def"], "keys": "2iab<C-o>0cd<Esc>j03."},
    {"name": "dot_ctrl_g_u_keeps_one_change", "lines": ["abc", "def"], "keys": "iab<C-g>ucd<Esc>j0."},
    #     DOT-REPEAT of commands that fell into the change_kind / with_count wildcards (#793).
    {"name": "dot_search_delete", "lines": ["aXbXcXdXe"], "keys": "xd/X<CR>."},
    {"name": "dot_search_change", "lines": ["aXbXcXdXe"], "keys": "c/X<CR>Q<Esc>l."},
    {"name": "dot_op_to_mark_char", "lines": ["abcdefghij"], "keys": "xlmalld`all."},
    {"name": "dot_op_to_mark_line", "lines": ["a", "b", "c", "d", "e", "f"], "keys": "majj>'ajj.",
     "setup": "set shiftwidth=4 expandtab startofline"},
    {"name": "dot_change_to_mark", "lines": ["foo bar baz qux"], "keys": "mawwc`aZ<Esc>."},
    {"name": "dot_replace_mode", "lines": ["abcdef", "ghijkl"], "keys": "xRXY<Esc>j0."},
    {"name": "dot_gi_insert", "lines": ["abc", "def"], "keys": "iX<Esc>jgiY<Esc>j."},
    {"name": "dot_count_join", "lines": ["a", "b", "c", "d", "e", "f", "g"], "keys": "J3."},
    {"name": "dot_count_join_nospace", "lines": ["a", "b", "c", "d", "e", "f", "g"], "keys": "gJ3."},
    {"name": "dot_count_increment", "lines": ["1 x"], "keys": "<C-a>5."},
    {"name": "dot_count_decrement", "lines": ["9 x"], "keys": "<C-x>5."},
    #     DOT-REPEAT of a Visual change (#794): `.` re-applies the operator to the same AMOUNT of text at the
    #     cursor (`:help visual-repeat`): same char count within a line, same line count linewise, same
    #     line count + end column for multi-line charwise. A count on `.` is ignored.
    {"name": "dot_visual_delete", "lines": ["abcdefgh"], "keys": "vld."},
    {"name": "dot_visual_line_delete", "lines": ["a", "b", "c", "d", "e"], "keys": "Vd."},
    {"name": "dot_visual_change", "lines": ["abcdefgh"], "keys": "vlcX<Esc>l."},
    {"name": "dot_visual_replace_char", "lines": ["abcdefgh"], "keys": "vlrxll."},
    {"name": "dot_visual_upcase", "lines": ["abcdefgh"], "keys": "vlUll."},
    {"name": "dot_visual_toggle_case", "lines": ["abcdefgh"], "keys": "vl~ll."},
    {"name": "dot_visual_multiline_char", "lines": ["abcd", "efgh", "ijkl", "mnop"], "keys": "lvjd."},
    {"name": "dot_visual_block_delete", "lines": ["abcd", "efgh", "ijkl", "mnop"], "keys": "l<C-v>jld."},
    {"name": "dot_visual_count_ignored", "lines": ["abcdefghijklmnop"], "keys": "vlld3."},
    {"name": "dot_visual_line_count_ignored", "lines": ["a", "b", "c", "d", "e", "f", "g", "h"], "keys": "Vjd2."},
    #     MIXED realistic refactors (navigate then edit; find-delimiter then change-inner) ---------------
    {"name": "refactor_2word_change_end", "lines": ["aa bb cc dd"], "keys": "2wceHELLO<Esc>0"},
    {"name": "refactor_find_paren_change_inner", "lines": ["foo(bar)baz"], "keys": "f(ci(X<Esc>"},
    #     SEARCH probe (one, per brief): `/` is not wired into the parity comparator's drive loop --------
    {"name": "search_then_daw_probe", "lines": ["hello world foo"], "keys": "/world<CR>daw"},
    # --- SEARCH corpus (impl/parity-search-drive): the comparator now drives `/pattern<CR>` by
    #     collecting the minibuffer pattern and applying SearchNext (mirroring main.rs). These exercise
    #     bare search moves, `n` repeat, and — as probes — search-as-operator-motion and a count prefix,
    #     which ruse's engine resets on `/` (findings if divergent). Every `expect` is oracle-captured.
    {"name": "search_word_move", "lines": ["hello world foo"], "keys": "/world<CR>"},
    {"name": "search_then_daw", "lines": ["hello world foo"], "keys": "/foo<CR>daw"},
    {"name": "search_next_n", "lines": ["foo bar foo bar foo"], "keys": "/foo<CR>n"},
    # PROBE — `d/pat<CR>` (search as an operator motion): ruse's engine resets the armed operator when
    # `/` opens the search line, so it performs a bare move where Vim deletes up to the match. Divergence
    # is the expected finding; the comparator stays green (divergence is data, never a failure).
    {"name": "search_as_dmotion", "lines": ["hello world foo"], "keys": "d/world<CR>"},
    # PROBE — `2/pat<CR>` (count before search = go to the Nth match in Vim): ruse resets the count on
    # `/`, so it lands on the FIRST match. Documented divergence.
    {"name": "search_count_prefix", "lines": ["foo bar foo bar foo"], "keys": "2/foo<CR>"},
    # --- SECTION motions (feat/section-motions): `]]`/`[[` seek a `{` (or form-feed) in column 0; `][`/`[]`
    #     seek a `}` (or form-feed). Exclusive motions → operators are linewise via the exclusive-linewise
    #     rule (shared with `d}`). Bare moves rest on the target line's first non-blank; clamp to the
    #     last/first line at the buffer edges. Every `expect` is oracle-captured from nvim v0.12.4.
    #     (Two nvim EOF quirks are DELIBERATELY not fixtured — a section-FORWARD operator running off the end
    #     of the file makes nvim's delete linewise-through-EOF but its yank charwise; ruse uses one span for
    #     d/y so both stop at the last line. Documented divergence in the change record.)
    {"name": "section_fwd_bare", "lines": ["{", "  a", "  b", "}", "{", "  c", "}"], "keys": "]]"},
    {"name": "section_fwd_count_clamps", "lines": ["{", "  a", "  b", "}", "{", "  c", "}"], "keys": "2]]"},
    {"name": "section_end_fwd_bare", "lines": ["{", "  a", "  b", "}", "{", "  c", "}"], "keys": "]["},
    {"name": "section_back_bare", "lines": ["{", "  a", "  b", "}", "{", "  c", "}"], "keys": "G[["},
    {"name": "section_end_back_bare", "lines": ["{", "  a", "  b", "}", "{", "  c", "}"], "keys": "G[]"},
    {"name": "section_fwd_eof_first_non_blank", "lines": ["{", "  a", "    xy"], "keys": "]]"},
    {"name": "section_fwd_form_feed", "lines": ["aaa", "\x0c", "bbb"], "keys": "]]"},
    {"name": "section_end_fwd_form_feed", "lines": ["aaa", "\x0c", "bbb"], "keys": "]["},
    {"name": "section_fwd_skips_indented_brace", "lines": ["a", " {", "b", "{", "c"], "keys": "]]"},
    {"name": "d_section_fwd_linewise", "lines": ["{", "  a", "  b", "}", "{", "  c", "}"], "keys": "d]]"},
    {"name": "d_section_end_fwd_linewise", "lines": ["{", "  a", "  b", "}", "{", "  c", "}"], "keys": "d]["},
    {"name": "y_section_fwd_linewise", "lines": ["{", "  a", "  b", "}", "{", "  c", "}"], "keys": "y]]"},
    {"name": "d_section_back_linewise", "lines": ["x", "{", "a", "}", "{", "b", "}"], "keys": "5Gd[["},
    {"name": "d_section_end_back_linewise", "lines": ["x", "{", "a", "}", "{", "b", "}"], "keys": "6Gd[]"},
    {"name": "y_section_back", "lines": ["x", "{", "a", "}", "{", "b", "}"], "keys": "6Gy[["},
    {"name": "d_section_back_bof_noop", "lines": ["x", "{", "a", "}"], "keys": "d[["},
    {"name": "section_fwd_from_mid", "lines": ["{", "a", "}", "{", "b", "}"], "keys": "jd]]"},
    # --- UNMATCHED-PAREN/BRACE motions (feat/unmatched-paren-motions): `[(`/`])` jump to the enclosing
    #     unmatched `(`/`)`; `[{`/`]}` to the enclosing unmatched `{`/`}`. Count steps out nesting levels.
    #     All four are EXCLUSIVE charwise (verified: `d])` does NOT eat the `)`, unlike `d%`), and take Vim's
    #     exclusive-linewise reduction when the target sits in column 0. Every `expect` is oracle-captured.
    #     bare moves ---------------------------------------------------------------------------------------
    {"name": "unmatched_paren_back_bare", "lines": ["(abcdef)"], "keys": "3l[("},
    {"name": "unmatched_paren_fwd_bare", "lines": ["(abcdef)"], "keys": "3l])"},
    {"name": "unmatched_brace_back_bare", "lines": ["{abcdef}"], "keys": "3l[{"},
    {"name": "unmatched_brace_fwd_bare", "lines": ["{abcdef}"], "keys": "3l]}"},
    #     nesting + count ----------------------------------------------------------------------------------
    {"name": "unmatched_paren_back_nested_inner", "lines": ["(a(b)c)"], "keys": "3l[("},
    {"name": "unmatched_paren_back_count_out_two", "lines": ["(a(b)c)"], "keys": "3l2[("},
    {"name": "unmatched_brace_fwd_nested_inner", "lines": ["{a{b}c}"], "keys": "3l]}"},
    {"name": "unmatched_brace_fwd_count_out_two", "lines": ["{a{b}c}"], "keys": "3l2]}"},
    #     cursor on the bracket itself → no-op; no enclosing bracket → no-op --------------------------------
    {"name": "unmatched_paren_back_on_open_noop", "lines": ["(abc)"], "keys": "[("},
    {"name": "unmatched_paren_fwd_on_close_noop", "lines": ["(abc)"], "keys": "$])"},
    {"name": "unmatched_paren_back_none_noop", "lines": ["abcdef"], "keys": "3l[("},
    #     multiline ----------------------------------------------------------------------------------------
    {"name": "unmatched_paren_back_multiline", "lines": ["foo(", "bar", ")baz"], "keys": "2G0[("},
    {"name": "unmatched_paren_fwd_multiline", "lines": ["foo(", "bar", ")baz"], "keys": "2G0])"},
    #     operators — exclusive charwise -------------------------------------------------------------------
    {"name": "d_unmatched_paren_back", "lines": ["(abcdef)"], "keys": "3ld[("},
    {"name": "d_unmatched_paren_fwd", "lines": ["(abcdef)"], "keys": "3ld])"},
    {"name": "y_unmatched_paren_fwd", "lines": ["(abcdef)"], "keys": "3ly])"},
    {"name": "c_unmatched_brace_fwd", "lines": ["{abcdef}"], "keys": "3lc]}X<Esc>"},
    {"name": "d_unmatched_brace_back", "lines": ["{abcdef}"], "keys": "3ld[{"},
    #     operator exclusive-linewise reduction at a column-0 target ----------------------------------------
    {"name": "d_unmatched_paren_fwd_linewise", "lines": ["foo(", "bar", ")baz"], "keys": "2G0d])"},
    #     operator no-op (no enclosing bracket) ------------------------------------------------------------
    {"name": "d_unmatched_paren_fwd_noop", "lines": ["abcdef"], "keys": "3ld])"},
    #     visual mode — selection is inclusive of both ends ------------------------------------------------
    {"name": "visual_unmatched_paren_fwd_delete", "lines": ["(abcdef)"], "keys": "3lv])d"},
    {"name": "visual_unmatched_paren_back_delete", "lines": ["(abcdef)"], "keys": "3lv[(d"},
    # --- METHOD (brace-block) motions (feat/method-motions): `]m`/`[m` next/prev method START (a `{`);
    #     `]M`/`[M` next/prev method END (a `}`). A faithful port of Vim's `nv_bracket_block` — pure brace
    #     navigation (ruse has no language model). Count-aware; exclusive charwise under an operator, sharing
    #     the exclusive-linewise reduction with the section / unmatched-brace motions (a column-0 landing
    #     deletes whole lines). VERIFIED against nvim v0.12.4 on flat / class / nested layouts. DIVERGENCE
    #     (documented non-goal, NO fixtures): braces inside string literals and comments are NOT skipped
    #     (ruse has no syntax model); nvim's `findmatch` skips them.
    #     CLASS = a two-method class; FLAT = top-level `{}` blocks.
    {"name": "method_start_fwd_bare", "lines": ["class Foo {", "    void a() {", "        x;", "    }", "    void b() {", "        y;", "    }", "}"], "keys": "]m"},
    {"name": "method_start_fwd_count2", "lines": ["class Foo {", "    void a() {", "        x;", "    }", "    void b() {", "        y;", "    }", "}"], "keys": "2]m"},
    {"name": "method_start_fwd_count3", "lines": ["class Foo {", "    void a() {", "        x;", "    }", "    void b() {", "        y;", "    }", "}"], "keys": "3]m"},
    {"name": "method_start_fwd_count4_class_close", "lines": ["class Foo {", "    void a() {", "        x;", "    }", "    void b() {", "        y;", "    }", "}"], "keys": "4]m"},
    {"name": "method_end_fwd_bare", "lines": ["class Foo {", "    void a() {", "        x;", "    }", "    void b() {", "        y;", "    }", "}"], "keys": "]M"},
    {"name": "method_end_fwd_count2", "lines": ["class Foo {", "    void a() {", "        x;", "    }", "    void b() {", "        y;", "    }", "}"], "keys": "2]M"},
    {"name": "method_start_fwd_from_inside", "lines": ["class Foo {", "    void a() {", "        x;", "    }", "    void b() {", "        y;", "    }", "}"], "keys": "2j]m"},
    {"name": "method_start_back_from_inside", "lines": ["class Foo {", "    void a() {", "        x;", "    }", "    void b() {", "        y;", "    }", "}"], "keys": "2j[m"},
    {"name": "method_end_fwd_from_inside", "lines": ["class Foo {", "    void a() {", "        x;", "    }", "    void b() {", "        y;", "    }", "}"], "keys": "2j]M"},
    {"name": "method_end_back_from_inside", "lines": ["class Foo {", "    void a() {", "        x;", "    }", "    void b() {", "        y;", "    }", "}"], "keys": "2j[M"},
    {"name": "method_start_back_count2_from_inside", "lines": ["class Foo {", "    void a() {", "        x;", "    }", "    void b() {", "        y;", "    }", "}"], "keys": "2j2[m"},
    {"name": "method_start_fwd_flat_top_level", "lines": ["void a() {", "    x;", "}", "void b() {", "    y;", "}"], "keys": "]m"},
    {"name": "method_start_fwd_flat_count2_close", "lines": ["void a() {", "    x;", "}", "void b() {", "    y;", "}"], "keys": "2]m"},
    {"name": "method_start_fwd_no_brace_noop", "lines": ["abc", "def"], "keys": "]m"},
    # operators — charwise mid-line landings and column-0 linewise reductions.
    {"name": "d_method_start_fwd_charwise", "lines": ["class Foo {", "    void a() {", "        x;", "    }", "    void b() {", "        y;", "    }", "}"], "keys": "2jd]m"},
    {"name": "d_method_end_back_charwise", "lines": ["class Foo {", "    void a() {", "        x;", "    }", "    void b() {", "        y;", "    }", "}"], "keys": "2jd[M"},
    {"name": "d_method_start_fwd_count4_linewise", "lines": ["class Foo {", "    void a() {", "        x;", "    }", "    void b() {", "        y;", "    }", "}"], "keys": "d4]m"},
    {"name": "d_method_start_fwd_flat_linewise", "lines": ["void a() {", "    x;", "}", "void b() {", "    y;", "}"], "keys": "d2]m"},
    {"name": "y_method_end_fwd", "lines": ["class Foo {", "    void a() {", "        x;", "    }", "    void b() {", "        y;", "    }", "}"], "keys": "y]M"},
    {"name": "c_method_start_fwd", "lines": ["class Foo {", "    void a() {", "        x;", "    }", "    void b() {", "        y;", "    }", "}"], "keys": "2jc]mZ<Esc>"},
    #     change/yank marks `[ `] '[ '] (issue #428) — the marks bound the last changed/yanked text.
    #     `[ / `] jump charwise to the first/last char; '[ / '] jump linewise to the first non-blank.
    {"name": "yank_word_mark_start", "lines": ["foo bar baz"], "keys": "wyiw$`["},
    {"name": "yank_word_mark_end", "lines": ["foo bar baz"], "keys": "wyiw0`]"},
    {"name": "insert_mark_start", "lines": ["abc"], "keys": "ihello<Esc>0`["},
    {"name": "insert_mark_end", "lines": ["abc"], "keys": "ihello<Esc>0`]"},
    {"name": "insert_mid_mark_end", "lines": ["abcdef"], "keys": "lliXY<Esc>0`]"},
    {"name": "put_line_mark_start", "lines": ["one", "two", "three"], "keys": "ddp`["},
    {"name": "put_line_mark_end", "lines": ["one", "two", "three"], "keys": "ddp`]"},
    {"name": "delete_char_mark_start", "lines": ["abcdef"], "keys": "llx`["},
    {"name": "delete_char_mark_end", "lines": ["abcdef"], "keys": "llx`]"},
    {"name": "replace_char_mark_end", "lines": ["abcdef"], "keys": "llrZ`]"},
    {"name": "quote_mark_start_line", "lines": ["  aa", "  bb", "cc"], "keys": "ggVjy'["},
    {"name": "quote_mark_end_line", "lines": ["  aa", "  bb", "cc"], "keys": "ggVjy']"},
    {"name": "multiline_change_mark_start", "lines": ["alpha", "beta", "gamma"], "keys": "2ccabc<Esc>`["},
    {"name": "multiline_change_mark_end", "lines": ["alpha", "beta", "gamma"], "keys": "2ccabc<Esc>`]"},
    {"name": "linewise_yank_mark_end", "lines": ["one", "two"], "keys": "yy`]"},
    {"name": "open_line_mark_start", "lines": ["abc"], "keys": "ohi<Esc>`["},
    {"name": "open_line_mark_end", "lines": ["abc"], "keys": "ohi<Esc>`]"},
    #     VISUAL-SELECTION marks `< `> '< '> (issue #451) — set when Visual/Select is LEFT (Esc AND
    #     operator-completion), backed by the SAME depth-1 store as `gv`. Backtick = charwise (start/end
    #     char of the selection); quote = linewise (first non-blank of the first/last selected line).
    #     Charwise selection: `< = word start, `> = word end.
    {"name": "visual_mark_start_charwise", "lines": ["foo bar baz"], "keys": "wviw<Esc>`<"},
    {"name": "visual_mark_end_charwise", "lines": ["foo bar baz"], "keys": "wviw<Esc>`>"},
    #     Linewise selection: `< = col 0 of first line, `> = last char of last line; '<//'> = first non-blank.
    {"name": "visual_mark_start_line_linewise", "lines": ["  alpha", "beta", "  gamma"], "keys": "Vj<Esc>'<"},
    {"name": "visual_mark_end_line_linewise", "lines": ["  alpha", "beta", "  gamma"], "keys": "Vj<Esc>'>"},
    {"name": "visual_mark_start_backtick_linewise", "lines": ["  alpha", "beta", "  gamma"], "keys": "Vj<Esc>`<"},
    {"name": "visual_mark_end_backtick_linewise", "lines": ["  alpha", "beta", "  gamma"], "keys": "Vj<Esc>`>"},
    #     Marks reflect the selection AFTER an operator too (`viwd` sets `<`/`>` around the deleted region).
    {"name": "visual_mark_start_after_operator", "lines": ["foo bar baz"], "keys": "wviwd`<"},
    {"name": "visual_mark_end_after_operator", "lines": ["foo bar baz"], "keys": "wviwd`>"},
    #     Blockwise selection: `< = top corner, `> = bottom corner (byte-min/max of the two ends).
    {"name": "visual_mark_start_blockwise", "lines": ["abcde", "fghij", "klmno"], "keys": "l<C-v>jl<Esc>`<"},
    {"name": "visual_mark_end_blockwise", "lines": ["abcde", "fghij", "klmno"], "keys": "l<C-v>jl<Esc>`>"},
    #     gv restores the last selection AND its kind: linewise `gvd` deletes whole lines; block `gvd` deletes
    #     the block; a mark jump before `gv` does not disturb the store (`< then gvd still deletes the word).
    {"name": "gv_reselect_linewise", "lines": ["one", "two", "three"], "keys": "Vjygvd"},
    {"name": "gv_reselect_blockwise", "lines": ["abcde", "fghij", "klmno"], "keys": "l<C-v>jl<Esc>gvd"},
    {"name": "gv_after_mark_jump", "lines": ["foo bar baz"], "keys": "wviw<Esc>`<gvd"},
    #     BACKWARD SEARCH `?` (issue #431) — `?pat` lands on the match BEFORE the cursor (wrapping to end
    #     of buffer); `n`/`N` repeat RELATIVE to the last search's direction; `?` is an EXCLUSIVE motion
    #     under an operator. The `/foo` cases are forward-`n`/`N` REGRESSION guards. Cursor homes to {1,0},
    #     so each fixture first moves (`$`) to set up a match to the LEFT of the cursor.
    {"name": "back_search_prev_match", "lines": ["foo bar foo baz"], "keys": "$?foo<CR>"},
    {"name": "back_search_wraps_to_end", "lines": ["bar foo baz"], "keys": "?foo<CR>"},
    {"name": "back_search_then_n_backward", "lines": ["foo x foo x foo"], "keys": "$?foo<CR>n"},
    {"name": "back_search_then_N_forward", "lines": ["foo x foo x foo"], "keys": "$?foo<CR>N"},
    {"name": "fwd_search_then_n_forward", "lines": ["foo x foo x foo"], "keys": "/foo<CR>n"},
    {"name": "fwd_search_then_N_backward", "lines": ["foo x foo x foo"], "keys": "/foo<CR>N"},
    {"name": "back_search_count", "lines": ["foo x foo x foo x end"], "keys": "$2?foo<CR>"},
    {"name": "d_back_search_exclusive", "lines": ["foo bar baz qux"], "keys": "$d?bar<CR>"},
    {"name": "c_back_search_exclusive", "lines": ["foo bar baz qux"], "keys": "$c?bar<CR>Z<Esc>"},
    {"name": "y_back_search_then_paste", "lines": ["foo bar baz qux"], "keys": "$y?bar<CR>P"},
    {"name": "empty_back_search_reuses_pattern", "lines": ["foo bar foo baz"], "keys": "/bar<CR>$?<CR>"},
    #     SEARCH OFFSETS (#475) — `/pat/e`, `/pat/s+N`, `/pat/+N`, `?pat?…`, `n`/`N` reapply, operator
    #     wiseness (`e` inclusive, `s` exclusive, line offsets linewise). All verified against nvim
    #     v0.12.4 headless; the LINE offset lands on COLUMN 0 (Vim "column 1"), NOT the first non-blank —
    #     and nvim's `-u NONE` `nostartofline` puts a linewise-delete cursor at column 0 too, matching ruse.
    {"name": "so_end", "lines": ["hello world foo"], "keys": "/foo/e<CR>"},
    {"name": "so_end_minus", "lines": ["hello world foo"], "keys": "/foo/e-1<CR>"},
    {"name": "so_start", "lines": ["hello world foo"], "keys": "/foo/s<CR>"},
    {"name": "so_start_plus", "lines": ["hello world foo"], "keys": "/foo/s+2<CR>"},
    {"name": "so_start_minus", "lines": ["hello world foo"], "keys": "/foo/s-1<CR>"},
    {"name": "so_begin_synonym", "lines": ["hello world foo"], "keys": "/foo/b<CR>"},
    {"name": "so_end_crosses_line", "lines": ["ab foo", "cd ef"], "keys": "/foo/e+1<CR>"},
    {"name": "so_line_down", "lines": ["a foo b", "c d", "e f"], "keys": "/foo/+1<CR>"},
    {"name": "so_line_up", "lines": ["a b", "c foo d", "e f"], "keys": "/foo/-1<CR>"},
    {"name": "so_count_end", "lines": ["fooXfooZfoo"], "keys": "2/foo/e<CR>"},
    {"name": "so_n_reapplies_end", "lines": ["abc foo def foo"], "keys": "/foo/e<CR>n"},
    {"name": "so_n_reapplies_start_minus", "lines": ["abc foo def foo xyz"], "keys": "/foo/s-1<CR>n"},
    {"name": "so_N_reapplies_end", "lines": ["abc foo def foo"], "keys": "/foo/e<CR>N"},
    {"name": "so_back_end", "lines": ["foo bar foo baz"], "keys": "$?foo?e<CR>"},
    {"name": "so_d_end_inclusive", "lines": ["hello world foo"], "keys": "d/foo/e<CR>"},
    {"name": "so_d_start_exclusive", "lines": ["hello world foo"], "keys": "d/foo/s<CR>"},
    {"name": "so_d_line_linewise", "lines": ["a foo", "b", "c", "d"], "keys": "d/foo/+1<CR>"},
    {"name": "so_y_end_paste", "lines": ["hello world foo"], "keys": "y/foo/e<CR>P"},
    {"name": "so_c_end", "lines": ["hello world foo"], "keys": "c/foo/e<CR>Z<Esc>"},
    {"name": "so_d_back_end_inclusive", "lines": ["third foobar line"], "keys": "$d?foobar?e<CR>"},
    {"name": "so_empty_reuse_offset", "lines": ["abc foo def foo"], "keys": "/foo/e<CR>//<CR>"},
    {"name": "so_empty_new_offset", "lines": ["abc foo def foo"], "keys": "/foo/e<CR>//s<CR>"},
    # Modifier handling (#598, #599, #1059): a CTRL chord is bound or inert, never its bare letter. A chord
    # nvim BEEPS on also flushes the rest of the fed keys, so those fixtures end at the chord and observe the
    # cursor (interactively the next typed key would still run).
    {"name": "chord_normal_c_k_inert", "lines": ["ab", "cd"], "keys": "jl<C-k>"},
    {"name": "chord_normal_c_s_inert", "lines": ["abc"], "keys": "l<C-s>x"},
    {"name": "chord_normal_c_h_left", "lines": ["abc"], "keys": "ll<C-h>x"},
    {"name": "chord_normal_c_j_down", "lines": ["ab", "cd"], "keys": "<C-j>x"},
    {"name": "chord_normal_c_n_down", "lines": ["ab", "cd", "ef"], "keys": "2<C-n>x"},
    {"name": "chord_op_d_c_j_linewise", "lines": ["a", "b", "c"], "keys": "d<C-j>"},
    {"name": "chord_op_d_c_h", "lines": ["abc"], "keys": "ld<C-h>"},
    {"name": "chord_op_d_c_k_aborts", "lines": ["ab", "cd"], "keys": "jd<C-k>"},
    {"name": "chord_op_d_c_q_blockwise", "lines": ["abc", "def"], "keys": "d<C-q>j"},
    {"name": "chord_visual_c_k_inert", "lines": ["abcd", "efgh"], "keys": "jvl<C-k>"},
    {"name": "chord_visual_c_p_up", "lines": ["abcd", "efgh"], "keys": "jv<C-p>d"},
    {"name": "chord_find_c_a_aborts", "lines": ["xa"], "keys": "f<C-a>"},
    {"name": "chord_g_c_e_aborts", "lines": ["ab cd"], "keys": "$g<C-e>"},
    {"name": "chord_insert_c_h_backspace", "lines": ["x"], "keys": "iab<C-h>c<Esc>"},
    {"name": "chord_insert_c_j_newline", "lines": ["x"], "keys": "ia<C-j>b<Esc>"},
    {"name": "chord_insert_c_s_inert", "lines": ["x"], "keys": "ia<C-s>b<Esc>"},
    {"name": "chord_insert_c_rbracket_inert", "lines": ["x"], "keys": "ia<C-]>b<Esc>"},
    {"name": "chord_insert_c_q_literal", "lines": ["x"], "keys": "i<C-q>065<Esc>"},
    # Session intercepts must leave a key to the engine while it waits for one (#679), drop the count and
    # armed operator they consume (#850), and the macro layer must not take an argument `q`/`@` (#851).
    {"name": "icpt_find_z", "lines": ["abc [z zHLq&@ xyz"], "keys": "fzx"},
    {"name": "icpt_find_H", "lines": ["abc [z zHLq&@ xyz", "next"], "keys": "fHx"},
    {"name": "icpt_find_L", "lines": ["abc [z zHLq&@ xyz", "next"], "keys": "fLx"},
    {"name": "icpt_find_q", "lines": ["abc [z zHLq&@ xyz"], "keys": "fqx"},
    {"name": "icpt_find_at", "lines": ["abc [z zHLq&@ xyz"], "keys": "f@x"},
    {"name": "icpt_find_amp", "lines": ["abc [z zHLq&@ xyz"], "keys": "f&x"},
    {"name": "icpt_till_z", "lines": ["abc [z zHLq&@ xyz"], "keys": "tzx"},
    {"name": "icpt_back_find_z", "lines": ["abc [z zHLq&@ xyz"], "keys": "$Fzx"},
    {"name": "icpt_find_bracket_then_zz", "lines": ["abc [z zHLq&@ xyz"], "keys": "f[zzx"},
    {"name": "icpt_replace_H", "lines": ["abc"], "keys": "rH"},
    {"name": "icpt_replace_M", "lines": ["abc"], "keys": "rM"},
    {"name": "icpt_replace_q", "lines": ["abc"], "keys": "rqx"},
    {"name": "icpt_replace_at", "lines": ["abc"], "keys": "r@x"},
    {"name": "icpt_replace_amp", "lines": ["abc"], "keys": "r&"},
    {"name": "icpt_replace_bracket_zz", "lines": ["abc"], "keys": "r[zzx"},
    {"name": "icpt_dfL_multiline", "lines": ["abLdef", "line2", "line3", "line4 c", "line5 d"], "keys": "dfL"},
    {"name": "icpt_dtM", "lines": ["abMdef", "line2", "line3"], "keys": "dtM"},
    {"name": "icpt_ctL", "lines": ["abLdef", "line2"], "keys": "ctLX<Esc>"},
    {"name": "icpt_dtq", "lines": ["abqdef"], "keys": "dtq"},
    {"name": "icpt_dtz", "lines": ["abcz def"], "keys": "dtz"},
    {"name": "icpt_dfq", "lines": ["abqdef"], "keys": "dfq"},
    {"name": "icpt_mark_z", "lines": ["abc def"], "keys": "mzw`zx"},
    {"name": "icpt_register_q", "lines": ["abc"], "keys": "\"qyy\"qP"},
    {"name": "icpt_register_z", "lines": ["abc"], "keys": "\"zyy\"zp"},
    {"name": "icpt_visual_replace_z", "lines": ["abc"], "keys": "vlrz"},
    {"name": "icpt_visual_find_z_d", "lines": ["abcz def"], "keys": "vfzd"},
    {"name": "icpt_gqj", "lines": ["aa", "bb", "cc"], "keys": "gqj"},
    {"name": "icpt_gqq_count", "lines": ["aa", "bb", "cc"], "keys": "2gqq"},
    {"name": "icpt_gqq_count_mid", "lines": ["aa", "bb", "cc", "dd"], "keys": "j3gqq"},
    {"name": "icpt_gqgq", "lines": ["aa", "bb", "cc"], "keys": "gqgq"},
    {"name": "icpt_g_amp", "lines": ["a a", "a a"], "keys": ":s/a/b/g<CR>g&"},
    {"name": "icpt_amp_count", "lines": ["a a", "a a", "a a"], "keys": ":s/a/b/<CR>j2&"},
    {"name": "icpt_amp_count_past_end", "lines": ["a a", "a a", "a a"], "keys": ":s/a/b/<CR>j5&"},
    {"name": "icpt_count_zz", "lines": ["alpha beta"] * 10, "keys": "5zzx"},
    {"name": "icpt_count_zz_keeps_column", "lines": ["alpha beta"] * 10, "keys": "lll5zzx"},
    {"name": "icpt_count_zt", "lines": ["alpha beta"] * 10, "keys": "j3ztx"},
    {"name": "icpt_count_z_cr", "lines": ["  alpha beta"] * 10, "keys": "lll3z<CR>x"},
    {"name": "icpt_count_q_dropped", "lines": ["foo bar baz"], "keys": "3qaxq"},
    {"name": "icpt_count_q_stop_dropped", "lines": ["foo bar baz"], "keys": "qax3qx"},
    {"name": "icpt_record_find_q", "lines": ["abqcq"], "keys": "qqfqxq"},
    {"name": "icpt_record_insert_q", "lines": ["alpha"], "keys": "qaiq<Esc>q"},
    {"name": "icpt_record_stop_in_visual", "lines": ["alpha beta"], "keys": "qavlqx"},
    # --- #605 Normal-mode special keys: X, <Del>, <Home>/<End>, <Space>/<BS> ('whichwrap' b,s), <Insert>.
    #     `X` = `dh` into the unnamed + small-delete registers; it never crosses to the previous line.
    #     `{count}<Del>` drops the last count digit; a bare `<Del>` is `x`. `<Space>`/`<BS>` wrap across lines
    #     as bare motions (nvim's default 'whichwrap' is "b,s"); under an operator `<Space>` stays on the line
    #     like `l` while `<BS>` crosses the newline (`d<BS>` at column 0 joins). -------------------------------
    #     `h`/`l` never leave the line (only `<Space>`/`<BS>` are in nvim's default 'whichwrap'): a count past
    #     the edge stops there, and under an operator the span is clamped to the line. -------------------------
    {"name": "l_stops_at_last_char", "lines": ["abc", "def"], "keys": "lll"},
    {"name": "count_l_stops_at_last_char", "lines": ["abc", "def"], "keys": "l5l"},
    {"name": "h_stops_at_col0", "lines": ["abc", "def"], "keys": "jh"},
    {"name": "count_h_stops_at_col0", "lines": ["abc", "def"], "keys": "jl5h"},
    {"name": "dh_at_col0_noop", "lines": ["abc", "def"], "keys": "jdh"},
    {"name": "d_count_l_stays_on_line", "lines": ["abc", "def"], "keys": "ld3l"},
    {"name": "count_s_stays_on_line", "lines": ["abc", "def"], "keys": "l3sX<Esc>"},
    {"name": "v_l_stops_at_last_char", "lines": ["abc", "def"], "keys": "vllld"},
    {"name": "X_delete_before", "lines": ["abcdef"], "keys": "3lX"},
    {"name": "X_count", "lines": ["abcdef"], "keys": "3l2X"},
    {"name": "X_count_past_bol", "lines": ["abcdef"], "keys": "3l9X"},
    {"name": "X_at_col0_noop", "lines": ["abcdef"], "keys": "X"},
    {"name": "X_no_line_cross", "lines": ["abc", "def"], "keys": "jX"},
    {"name": "X_dot", "lines": ["abcdef"], "keys": "3lX."},
    {"name": "del_is_x", "lines": ["abcdef"], "keys": "3l<Del>"},
    {"name": "del_drops_count_digit", "lines": ["abcdef"], "keys": "l23<Del>x"},
    {"name": "del_drops_only_digit", "lines": ["abcdef"], "keys": "l3<Del>x"},
    {"name": "del_dot", "lines": ["abcdef"], "keys": "3l<Del>."},
    {"name": "home_is_0", "lines": ["  abc"], "keys": "4l<Home>"},
    {"name": "end_is_dollar", "lines": ["abc def"], "keys": "l<End>"},
    {"name": "end_count", "lines": ["abc", "def", "ghi"], "keys": "l2<End>"},
    {"name": "dollar_count", "lines": ["abc", "def", "ghi"], "keys": "l2$"},
    {"name": "d_dollar_count", "lines": ["abc", "def", "ghi"], "keys": "ld2$"},
    {"name": "d_dollar_count_past_eof", "lines": ["abc", "def"], "keys": "ld5$"},
    {"name": "dollar_count_past_eof", "lines": ["abc", "def"], "keys": "l5$"},
    {"name": "d_end", "lines": ["abcdef"], "keys": "3ld<End>"},
    {"name": "d_home", "lines": ["abcdef"], "keys": "3ld<Home>"},
    {"name": "space_wraps", "lines": ["abc", "def"], "keys": "2l<Space>"},
    {"name": "space_count_wraps", "lines": ["abc", "def"], "keys": "l3<Space>"},
    {"name": "space_last_char_stays", "lines": ["abc", "def"], "keys": "j2l<Space>"},
    {"name": "d_space_stays_on_line", "lines": ["abc", "def"], "keys": "2ld<Space>"},
    {"name": "d_count_space_stays_on_line", "lines": ["abc", "def"], "keys": "ld3<Space>"},
    {"name": "bs_wraps", "lines": ["abc", "def"], "keys": "j<BS>"},
    {"name": "bs_count_wraps", "lines": ["abc", "def"], "keys": "jl3<BS>"},
    {"name": "bs_at_bof_stays", "lines": ["abc", "def"], "keys": "<BS>"},
    {"name": "d_bs_joins", "lines": ["abc", "def"], "keys": "jd<BS>"},
    {"name": "d_count_bs_crosses", "lines": ["abc", "def"], "keys": "jld3<BS>"},
    {"name": "insert_key_enters_insert", "lines": ["abcdef"], "keys": "3l<Insert>X<Esc>"},
    # --- #607 Insert-mode cursor keys. Arrows/<Home>/<End> move without leaving Insert, never wrap, BREAK
    #     the undo sequence and START a new insert (so `.` repeats only what was typed after the move; the
    #     undo half is unit-tested, since the oracle's own `nvim_buf_set_lines` is undoable).
    #     <Up>/<Down> keep the wanted column. <Del> deletes under the caret, joining at end of line, and
    #     stays part of the insert (one undo step, replayed by `.`). <Insert> toggles Insert<->Replace. ------
    {"name": "i_right", "lines": ["abc"], "keys": "lix<Right>y<Esc>"},
    {"name": "i_left", "lines": ["abc"], "keys": "lix<Left>y<Esc>"},
    {"name": "i_right_dot_repeats_tail", "lines": ["abc"], "keys": "lix<Right>y<Esc>."},
    {"name": "i_end", "lines": ["abc"], "keys": "lix<End>y<Esc>"},
    {"name": "i_home", "lines": ["abc"], "keys": "lix<Home>y<Esc>"},
    {"name": "i_down_short_line", "lines": ["abcdef", "ab", "abcdef"], "keys": "4li<Down>x<Esc>"},
    {"name": "i_down_keeps_column", "lines": ["abcdef", "ab", "abcdef"], "keys": "4li<Down><Down>x<Esc>"},
    {"name": "i_up_short_line", "lines": ["abcdef", "ab", "abcdef"], "keys": "jj4li<Up>x<Esc>"},
    {"name": "i_del", "lines": ["abc", "def"], "keys": "li<Del>x<Esc>"},
    {"name": "a_del_joins_at_eol", "lines": ["abc", "def"], "keys": "lla<Del>x<Esc>"},
    {"name": "a_del_twice", "lines": ["abc", "def"], "keys": "lla<Del><Del>x<Esc>"},
    {"name": "a_del_dot", "lines": ["abc", "def"], "keys": "llax<Del><Esc>."},
    {"name": "i_right_stops_at_eol", "lines": ["abc"], "keys": "lli<Right><Right>x<Esc>"},
    {"name": "i_left_stops_at_bol", "lines": ["abc"], "keys": "i<Left>x<Esc>"},
    {"name": "i_left_no_wrap", "lines": ["abc", "def"], "keys": "ji<Left>x<Esc>"},
    {"name": "a_right_no_wrap", "lines": ["abc", "def"], "keys": "lla<Right>x<Esc>"},
    {"name": "i_insert_toggles_replace", "lines": ["abc"], "keys": "i<Insert>XY<Esc>"},
    {"name": "i_insert_toggles_back", "lines": ["abc"], "keys": "i<Insert>XY<Insert>Z<Esc>"},
    {"name": "R_insert_toggles_insert", "lines": ["abc"], "keys": "RX<Insert>Y<Esc>"},
    # --- #611 quit in a split closes only the focused window: the editor keeps running, so the key after
    #     it still edits (ruse's session ignores keys once it has quit). Never on the LAST window, which
    #     would end the oracle process. ------------------------------------------------------------------
    {"name": "split_q_closes_window", "lines": ["abc"], "keys": ":split<CR>:q<CR>x"},
    {"name": "split_q_modified_closes_window", "lines": ["abc"], "keys": ":split<CR>x:q<CR>x"},
    {"name": "split_q_bang_closes_window", "lines": ["abc"], "keys": ":split<CR>x:q!<CR>x"},
    {"name": "split_zq_closes_window", "lines": ["abc"], "keys": ":split<CR>xZQx"},
    {"name": "split_quit_closes_window", "lines": ["abc"], "keys": ":split<CR>:quit<CR>x"},
    {"name": "split_ctrl_w_q_closes_window", "lines": ["abc"], "keys": ":split<CR><C-w>qx"},
    {"name": "split_close_closes_window", "lines": ["abc"], "keys": ":split<CR>:close<CR>x"},
    {"name": "split_hide_closes_window", "lines": ["abc"], "keys": ":split<CR>:hide<CR>x"},
    #     OPERATOR / COUNT / PENDING-STATE GRAMMAR (#789 #790 #791 #796 #797 #799 #800). Every count
    #     segment multiplies — before a register prefix, before the operator, after it — and a key that
    #     cannot continue an armed operator aborts it. CAUTION for the abort fixtures: `feedkeys(..., 'x')`
    #     FLUSHES the remaining typeahead on the abort's beep, so each one ENDS at the aborting key (the
    #     typed-key behaviour after an abort was probed separately over RPC, one key per input).
    {"name": "cnt_doubled_after_op", "lines": ["l1", "l2", "l3", "l4", "l5", "l6"], "keys": "d3d"},
    {"name": "cnt_doubled_both", "lines": ["l1", "l2", "l3", "l4", "l5", "l6"], "keys": "2d2d"},
    {"name": "cnt_doubled_yank_paste", "lines": ["l1", "l2", "l3", "l4", "l5", "l6"], "keys": "y2yP"},
    {"name": "cnt_doubled_change", "lines": ["l1", "l2", "l3", "l4", "l5", "l6"], "keys": "c2cX<Esc>"},
    {"name": "cnt_doubled_shift", "lines": ["l1", "l2", "l3", "l4"], "keys": "2>2>", "setup": "set shiftwidth=4 expandtab startofline"},
    {"name": "cnt_doubled_case", "lines": ["ab", "cd", "ef", "gh"], "keys": "2gU2U"},
    {"name": "cnt_doubled_case_g_form", "lines": ["ab", "cd", "ef", "gh"], "keys": "gU2gU"},
    {"name": "cnt_doubled_toggle_g_form", "lines": ["ab", "cd", "ef", "gh"], "keys": "g~g~"},
    {"name": "cnt_before_op_G", "lines": ["one two three", "line2 a", "line3 b", "line4 c", "line5 d"], "keys": "3j2dG"},
    {"name": "cnt_after_op_G", "lines": ["one two three", "line2 a", "line3 b", "line4 c", "line5 d"], "keys": "3jd2G"},
    {"name": "cnt_both_op_G", "lines": ["one two three", "line2 a", "line3 b", "line4 c", "line5 d"], "keys": "3j2d3G"},
    {"name": "cnt_before_op_G_yank", "lines": ["one two three", "line2 a", "line3 b", "line4 c", "line5 d"], "keys": "3j2yGP"},
    {"name": "cnt_before_op_percent", "lines": ["one two three", "line2 a", "line3 b", "line4 c", "line5 d"], "keys": "3j50d%"},
    {"name": "cnt_before_op_gg", "lines": ["one two three", "line2 a", "line3 b", "line4 c", "line5 d"], "keys": "3j2dgg"},
    {"name": "abort_y_then_d", "lines": ["foo bar baz qux"], "keys": "yd"},
    {"name": "abort_y_then_x", "lines": ["foo bar baz qux"], "keys": "yx"},
    {"name": "abort_y_then_D", "lines": ["foo bar baz qux"], "keys": "yD"},
    {"name": "abort_d_then_J", "lines": ["foo bar baz qux", "two"], "keys": "dJ"},
    {"name": "abort_y_then_tilde", "lines": ["foo bar baz qux"], "keys": "y~"},
    {"name": "abort_d_then_p", "lines": ["foo bar baz qux", "two"], "keys": "yyjdp"},
    {"name": "abort_y_then_u", "lines": ["foo bar baz qux"], "keys": "xyu"},
    {"name": "abort_d_then_s", "lines": ["foo bar baz qux"], "keys": "ds"},
    {"name": "abort_d_then_dot", "lines": ["foo bar baz qux"], "keys": "xd."},
    {"name": "abort_d_then_gJ", "lines": ["foo bar baz qux", "two"], "keys": "dgJ"},
    {"name": "abort_gU_then_d", "lines": ["foo bar baz qux"], "keys": "gUd"},
    {"name": "abort_d_then_bracket_p", "lines": ["foo bar baz qux"], "keys": "yyd]p"},
    {"name": "abort_d_then_ctrl_a", "lines": ["foo 12 baz"], "keys": "d<C-a>"},
    {"name": "reg_count_before_prefix_yank", "lines": ["l1", "l2", "l3", "l4", "l5"], "keys": "3\"ayy\"aP"},
    {"name": "reg_count_before_prefix_x", "lines": ["abcdef"], "keys": "2\"ax"},
    {"name": "reg_count_after_prefix_x", "lines": ["abcdef"], "keys": "\"a2x"},
    {"name": "reg_count_segments_multiply", "lines": ["abcdefghijklmnopqrstuvwxyz0123456789"], "keys": "2\"a3d2l"},
    {"name": "reg_one_shot_paste", "lines": ["one two three", "line2 a", "line3 b"], "keys": "j\"ayyki<C-o>\"ap<Esc>"},
    {"name": "reg_one_shot_count_yank", "lines": ["one two three", "line2 a", "line3 b"], "keys": "ji<C-o>2\"ayy<Esc>\"aP"},
    {"name": "n_count", "lines": ["a1", "a2", "a3", "a4", "a5", "a6"], "keys": "/a<CR>3nx"},
    {"name": "N_count", "lines": ["a1", "a2", "a3", "a4", "a5", "a6"], "keys": "/a<CR>G2Nx"},
    {"name": "n_under_d_exclusive_linewise", "lines": ["foo bar", "x", "foo"], "keys": "/foo<CR>ggdn"},
    {"name": "n_under_c", "lines": ["foo", "foo"], "keys": "/foo<CR>ggcnZ<Esc>"},
    {"name": "n_under_d_count", "lines": ["a b a b a b a"], "keys": "/a<CR>0d2n"},
    {"name": "n_under_d_op_count", "lines": ["a b a b a b a"], "keys": "/a<CR>02dn"},
    {"name": "N_under_d", "lines": ["a b a b a b a"], "keys": "/a<CR>$dN"},
    {"name": "n_under_y_paste", "lines": ["a b a b a b a"], "keys": "/a<CR>0ynP"},
    {"name": "n_under_gU", "lines": ["a b a b a b a"], "keys": "/a<CR>0gUn"},
    {"name": "gn_under_gU", "lines": ["a b a b a b a"], "keys": "/b<CR>0gUgn"},
    {"name": "gn_under_gU_dot", "lines": ["a b a b a b a"], "keys": "/b<CR>0gUgn."},
    {"name": "gn_under_d_op_count", "lines": ["a b a b a b a"], "keys": "/b<CR>02dgn"},
    {"name": "search_under_gU", "lines": ["foo bar baz qux"], "keys": "gU/bar<CR>"},
    {"name": "search_under_toggle", "lines": ["foo bar baz qux"], "keys": "g~/baz<CR>"},
    {"name": "search_under_shift", "lines": ["a", "qq", "b"], "keys": ">/qq<CR>", "setup": "set shiftwidth=4 expandtab startofline"},
    {"name": "search_under_shift_count", "lines": ["a", "qq", "b"], "keys": "2>/b<CR>", "setup": "set shiftwidth=4 expandtab startofline"},
    {"name": "mark_under_gU_charwise", "lines": ["foo bar", "baz qux"], "keys": "jmakgU`a"},
    {"name": "gn_under_shift", "lines": ["a", "b x", "c"], "keys": "/x<CR>gg>gn", "setup": "set shiftwidth=4 expandtab startofline"},
    {"name": "doubled_format_g_form", "lines": [" ".join(["word"] * 24)], "keys": "gqgq"},
    {"name": "mark_under_format_linewise", "lines": [" ".join(["word"] * 24), "x"], "keys": "jmakgq'a"},
    {"name": "search_under_format", "lines": [" ".join(["word"] * 24), "end"], "keys": "gq/end<CR>"},
    {"name": "cnt_insert_ctrl_v_esc", "lines": ["x"], "keys": "3i<C-v>65<Esc>"},
    {"name": "cnt_insert_text_ctrl_v_esc", "lines": ["x"], "keys": "3iab<C-v>65<Esc>"},
    {"name": "ctrl_v_then_ctrl_a", "lines": ["x"], "keys": "ihi<Esc>i<C-v>65<C-a><Esc>"},
    {"name": "cnt_insert_ctrl_a", "lines": ["x"], "keys": "ihi<Esc>3iab<C-a><Esc>"},
    {"name": "op_d_space_wraps", "lines": ["ab", "cd"], "keys": "l2d<Space>"},
    {"name": "op_d_bs_wraps", "lines": ["ab", "cd"], "keys": "jd2<BS>"},
    {"name": "op_d_count_del_edits_count", "lines": ["foo bar baz qux"], "keys": "d23<Del>w"},
    {"name": "abort_d_then_X", "lines": ["foo bar baz qux"], "keys": "$dX"},
    # --- #601 `|` chains Ex commands: `:s` splits only after its flags (a `|` in the pattern/replacement is
    #     literal), and a failing command drops the rest of the line. -----------------------------------
    {"name": "bar_sub_then_delete", "lines": ["a a", "a a", "x"], "keys": ":s/a/b/|d<CR>"},
    {"name": "bar_sub_flags_then_join", "lines": ["a a", "a a", "x"], "keys": ":s/a/b/g|j<CR>"},
    {"name": "bar_sub_spaced", "lines": ["a a", "a a", "x"], "keys": ":s/a/b/ | j<CR>"},
    {"name": "bar_sub_chain", "lines": ["a a", "a a", "x"], "keys": ":s/a/b/|s/a/c/<CR>"},
    {"name": "bar_whole_file_sub_then_join", "lines": ["a a", "a a", "x"], "keys": ":%s/a/b/|j<CR>"},
    {"name": "bar_failed_sub_drops_the_rest", "lines": ["a a", "a a", "x"], "keys": ":s/z/b/|d<CR>"},
    {"name": "bar_in_sub_pattern_is_literal", "lines": ["a|b a", "a a"], "keys": ":s/a|b/X/<CR>"},
    # `:j` (unlike `J`) leaves the cursor on the joined line's first non-blank.
    {"name": "ex_join_cursor_first_non_blank", "lines": ["  a b", "c"], "keys": "w:j<CR>"},
    {"name": "bar_in_sub_replacement_is_literal", "lines": ["a a", "a a"], "keys": ":s/a/X|Y/<CR>"},
    # --- #601 `:sig` is `:sign` in Vim (E471 here), never `:s` with the flags `ig`. -----------------------
    {"name": "sig_is_not_repeat_substitute", "lines": ["a a", "a a", "x"], "keys": ":s/a/b/<CR>j:sig<CR>"},
    {"name": "si_repeats_substitute_ignoring_case", "lines": ["a a", "A a", "x"], "keys": ":s/a/b/<CR>j:si<CR>"},
    # --- #601 `tabstop` and `shiftwidth` are distinct. The options are set in the KEYS (not `setup`) so ruse,
    #     whose defaults are ts=sw=4 + expandtab, runs the same configuration; `startofline` only pins nvim's
    #     `>>` cursor rule to ruse's. ---------------------------------------------------------------------
    {"name": "ts_sw_apart_shift_noet_once", "lines": ["x"], "keys": ":set ts=8<CR>:set sw=4<CR>:set noet<CR>>>", "setup": "set startofline"},
    {"name": "ts_sw_apart_shift_noet_twice", "lines": ["x"], "keys": ":set ts=8<CR>:set sw=4<CR>:set noet<CR>>>>>", "setup": "set startofline"},
    {"name": "ts_sw_apart_shift_et_expands_tab", "lines": ["\tx"], "keys": ":set ts=8<CR>:set sw=4<CR>:set et<CR>>>", "setup": "set startofline"},
    {"name": "ts_sw_apart_unshift_tab", "lines": ["\tx"], "keys": ":set ts=8<CR>:set sw=4<CR>:set noet<CR><<", "setup": "set startofline"},
    {"name": "set_ts_keeps_shiftwidth", "lines": ["x"], "keys": ":set sw=4<CR>:set ts=8<CR>:set et<CR>>>", "setup": "set startofline"},
    {"name": "sw_zero_follows_tabstop", "lines": ["x"], "keys": ":set ts=2<CR>:set sw=0<CR>:set et<CR>>>", "setup": "set startofline"},
    {"name": "ts_sw_bar_chain_shift", "lines": ["x"], "keys": ":set ts=8|set sw=4|set noet<CR>>>>>", "setup": "set startofline"},
    {"name": "ts_sw_apart_shift_motion", "lines": ["a", "\tb"], "keys": ":set ts=8<CR>:set sw=4<CR>:set noet<CR>>j", "setup": "set startofline"},
    {"name": "ts_sw_apart_ex_shift_twice", "lines": ["x"], "keys": ":set ts=8<CR>:set sw=4<CR>:set noet<CR>:>><CR>", "setup": "set startofline"},
    {"name": "ts_sw_apart_reindent", "lines": ["{", "a", "}"], "keys": ":set ts=8<CR>:set sw=4<CR>:set noet<CR>=G"},
    {"name": "i_tab_smarttab_indent_et", "lines": ["x"], "keys": ":set ts=3<CR>:set sw=2<CR>:set et<CR>i<Tab>y<Esc>"},
    {"name": "i_tab_smarttab_indent_noet", "lines": ["x"], "keys": ":set ts=8<CR>:set sw=4<CR>:set noet<CR>i<Tab>y<Esc>"},
    {"name": "i_tab_smarttab_twice_noet", "lines": ["x"], "keys": ":set ts=8<CR>:set sw=4<CR>:set noet<CR>i<Tab><Tab>y<Esc>"},
    {"name": "a_tab_midline_uses_tabstop_et", "lines": ["ab"], "keys": ":set ts=3<CR>:set sw=8<CR>:set et<CR>A<Tab>y<Esc>"},
    {"name": "a_tab_midline_noet_is_hard_tab", "lines": ["ab"], "keys": ":set ts=3<CR>:set sw=8<CR>:set noet<CR>A<Tab>y<Esc>"},
    {"name": "i_ctrl_t_rounds_to_shiftwidth", "lines": ["  x"], "keys": ":set ts=8<CR>:set sw=4<CR>:set noet<CR>A<C-t>y<Esc>"},
    {"name": "i_ctrl_t_twice_makes_a_tab", "lines": ["  x"], "keys": ":set ts=8<CR>:set sw=4<CR>:set noet<CR>A<C-t><C-t>y<Esc>"},
    {"name": "i_ctrl_d_rounds_to_shiftwidth", "lines": ["      x"], "keys": ":set ts=8<CR>:set sw=4<CR>:set noet<CR>A<C-d>y<Esc>"},
    {"name": "i_ctrl_t_caret_in_indent", "lines": ["    x"], "keys": ":set ts=8<CR>:set sw=4<CR>:set et<CR>lli<C-t>y<Esc>"},
    {"name": "gr_over_tab_at_ts8", "lines": ["a\tb"], "keys": ":set ts=8<CR>:set noet<CR>lgRXY<Esc>"},
    # --- #606 Visual operators and command entry: `:` opens `:'<,'>`, the linewise uppercase forms
    #     (`D`/`X`/`Y`/`C`/`S`/`R`; blockwise `D`/`C` run to end of line, `X`/`Y` stay blockwise), `J`/`gJ`,
    #     `>`/`<` with a count, `gq`, `!{filter}`, `O` and `CTRL-C`. -------------------------------------
    {"name": "v_colon_range_sub", "lines": ["aa", "aa", "aa"], "keys": "vj:s/a/X/<CR>"},
    {"name": "v_colon_range_delete", "lines": ["a", "b", "c"], "keys": "Vj:d<CR>"},
    {"name": "v_colon_block_range", "lines": ["a", "b", "c", "d"], "keys": "j<C-v>j:d<CR>"},
    {"name": "v_shift_right", "lines": ["a", "b", "c"], "keys": "vj>", "setup": "set shiftwidth=4 expandtab startofline"},
    {"name": "v_shift_right_count", "lines": ["a", "b", "c"], "keys": "vj2>", "setup": "set shiftwidth=4 expandtab startofline"},
    {"name": "v_shift_left", "lines": ["        a", "    b", "c"], "keys": "Vj<", "setup": "set shiftwidth=4 expandtab startofline"},
    {"name": "v_J_joins", "lines": ["a", "b", "c"], "keys": "vjJ"},
    {"name": "v_J_one_line_joins_two", "lines": ["a", "b", "c"], "keys": "vJ"},
    {"name": "v_gJ_joins_raw", "lines": ["a", "  b", "c"], "keys": "vjgJ"},
    {"name": "v_D_linewise", "lines": ["abc", "def", "ghi"], "keys": "lvjD"},
    {"name": "v_X_linewise", "lines": ["abc", "def", "ghi"], "keys": "lvjX"},
    {"name": "v_Y_linewise", "lines": ["abc", "def", "ghi"], "keys": "lvjY"},
    {"name": "v_C_linewise", "lines": ["abc", "def", "ghi"], "keys": "lvjCZ<Esc>"},
    {"name": "v_S_linewise", "lines": ["abc", "def", "ghi"], "keys": "lvjSZ<Esc>"},
    {"name": "v_R_linewise", "lines": ["abc", "def", "ghi"], "keys": "lvjRZ<Esc>"},
    {"name": "vb_D_to_eol", "lines": ["abcd", "efgh", "ijkl"], "keys": "l<C-v>jD"},
    {"name": "vb_X_blockwise", "lines": ["abcd", "efgh", "ijkl"], "keys": "l<C-v>jX"},
    {"name": "vb_Y_blockwise", "lines": ["abcd", "efgh", "ijkl"], "keys": "l<C-v>jY"},
    {"name": "vb_C_to_eol", "lines": ["abcd", "efgh", "ijkl"], "keys": "l<C-v>jCZ<Esc>"},
    {"name": "vb_S_linewise", "lines": ["abcd", "efgh", "ijkl"], "keys": "l<C-v>jSZ<Esc>"},
    {"name": "vb_R_linewise", "lines": ["abcd", "efgh", "ijkl"], "keys": "l<C-v>jRZ<Esc>"},
    {"name": "v_O_charwise_swaps", "lines": ["abcde"], "keys": "lvllOd"},
    {"name": "vb_O_moves_horizontally", "lines": ["abcd", "efgh", "ijkl"], "keys": "l<C-v>jlOhd"},
    {"name": "v_gq_reflows", "lines": ["aa", "bb", "cc"], "keys": "Vjgq"},
    {"name": "v_filter_sort", "lines": ["c", "b", "a"], "keys": "VG!sort<CR>"},
    {"name": "v_ctrl_c_leaves", "lines": ["abc"], "keys": "vl<C-c>x"},
    {"name": "v_tilde_toggles", "lines": ["abC"], "keys": "vl~"},
    {"name": "v_r_replaces", "lines": ["abc"], "keys": "vlrx"},
    {"name": "vb_I_inserts", "lines": ["abc", "def"], "keys": "l<C-v>jIZ<Esc>"},
    {"name": "vb_A_appends", "lines": ["abc", "def"], "keys": "l<C-v>jAZ<Esc>"},
    {"name": "gv_after_esc_and_move", "lines": ["abc def"], "keys": "vl<Esc>wgvd"},
    {"name": "gV_normal_is_noop", "lines": ["abc"], "keys": "gVx"},
    {"name": "V_d_keeps_column", "lines": ["abcd", "efgh", "ijkl"], "keys": "lVjd"},
    {"name": "v_D_keeps_wanted_column", "lines": ["abcd", "ef", "ijkl"], "keys": "lllvjD"},
    {"name": "v_dollar_D_to_eol_column", "lines": ["abcd", "efgh", "ijkl"], "keys": "jvj$D"},
    {"name": "v_C_keeps_indent", "lines": ["  abcd", "efgh", "ijkl"], "keys": "lvjCZ<Esc>"},
    {"name": "vb_dollar_d_ragged", "lines": ["abcd", "ef", "ijkl"], "keys": "l<C-v>jj$d"},
    {"name": "vb_D_ragged", "lines": ["abcd", "e", "ijkl"], "keys": "l<C-v>jjD"},
    {"name": "vb_X_narrow", "lines": ["abcd", "efgh", "ijkl"], "keys": "ll<C-v>jhX"},
    {"name": "vb_D_whole_rows", "lines": ["abcd", "efgh"], "keys": "l<C-v>jhD"},
    {"name": "V_J_three_lines", "lines": ["a", "b", "c", "d"], "keys": "VjjJ"},
    {"name": "v_gq_joins_short", "lines": ["aa bb", "cc"], "keys": "vjgq"},
    {"name": "v_shift_dot_same_lines", "lines": ["a", "b", "c", "d", "e"], "keys": "vj>jj.", "setup": "set shiftwidth=4 expandtab startofline"},
    {"name": "v_D_dot", "lines": ["a", "b", "c", "d", "e"], "keys": "vjDj."},
    {"name": "v_J_dot", "lines": ["a", "b", "c", "d", "e"], "keys": "vjJj."},
    {"name": "v_colon_esc_leaves_visual", "lines": ["a", "b"], "keys": "vj:<Esc>x"},
    {"name": "V_bang_filter_two", "lines": ["c", "b", "a"], "keys": "Vj!sort<CR>"},
    # --- #606/#792 Select mode: `gh`/`gH`/`g CTRL-H` enter it; a typed printable (any letter, digit,
    #     `"`, `:`, space, `<CR>`) replaces the selection and enters Insert; `<BS>`/`CTRL-H`/`<Del>` delete
    #     it; `CTRL-O` runs one Visual command; arrows and `<S-…>`/`<Home>`/`<End>` extend it. -----------
    {"name": "s_gh_type", "lines": ["abc"], "keys": "ghXY<Esc>"},
    {"name": "s_gh_count", "lines": ["abcdef"], "keys": "3ghX<Esc>"},
    {"name": "s_gH_type", "lines": ["abc", "def"], "keys": "gHX<Esc>"},
    {"name": "s_gH_count", "lines": ["abc", "def", "ghi"], "keys": "2gHX<Esc>"},
    {"name": "s_gH_last_line", "lines": ["abc", "def"], "keys": "jgHX<Esc>"},
    {"name": "s_g_ctrl_h_type", "lines": ["abc", "def"], "keys": "g<C-h>X<Esc>"},
    {"name": "s_g_ctrl_h_extend", "lines": ["abc", "def"], "keys": "g<C-h><S-Down><S-Right>X<Esc>"},
    {"name": "s_ctrl_g_type", "lines": ["foo bar baz qux"], "keys": "vl<C-g>hello<Esc>"},
    {"name": "s_ctrl_g_d", "lines": ["foo bar baz qux"], "keys": "vl<C-g>d<Esc>"},
    {"name": "s_ctrl_g_f", "lines": ["foo bar"], "keys": "vl<C-g>f<Esc>"},
    {"name": "s_ctrl_g_g", "lines": ["foo bar"], "keys": "vl<C-g>g<Esc>"},
    {"name": "s_ctrl_g_digit", "lines": ["foo bar"], "keys": "vl<C-g>5<Esc>"},
    {"name": "s_ctrl_g_z", "lines": ["foo bar"], "keys": "vl<C-g>z<Esc>"},
    {"name": "s_ctrl_g_quote", "lines": ["foo bar"], "keys": 'vl<C-g>"<Esc>'},
    {"name": "s_ctrl_g_colon", "lines": ["foo bar"], "keys": "vl<C-g>:<Esc>"},
    {"name": "s_ctrl_g_space", "lines": ["foo bar"], "keys": "vl<C-g> <Esc>"},
    {"name": "s_ctrl_g_toggles_back", "lines": ["foo bar"], "keys": "vl<C-g><C-g>d"},
    {"name": "s_bs_deletes", "lines": ["foo bar"], "keys": "vl<C-g><BS>x"},
    {"name": "s_ctrl_h_deletes", "lines": ["foo bar"], "keys": "vl<C-g><C-h>x"},
    {"name": "s_del_deletes", "lines": ["foo bar"], "keys": "vl<C-g><Del>x"},
    {"name": "s_cr_replaces", "lines": ["foo bar"], "keys": "vl<C-g><CR><Esc>"},
    {"name": "s_ctrl_o_one_visual_cmd", "lines": ["foo bar baz"], "keys": "vl<C-g><C-o>eX<Esc>"},
    {"name": "s_ctrl_o_d", "lines": ["foo bar baz"], "keys": "vl<C-g><C-o>d"},
    {"name": "s_right_extends", "lines": ["abcdef"], "keys": "gh<Right><Right>X<Esc>"},
    {"name": "s_left_extends", "lines": ["abcdef"], "keys": "$gh<Left>X<Esc>"},
    {"name": "s_down_extends", "lines": ["abc", "def"], "keys": "gh<Down>X<Esc>"},
    {"name": "s_shift_right_extends_word", "lines": ["abc def ghi"], "keys": "gh<S-Right>X<Esc>"},
    {"name": "s_end_extends", "lines": ["abc def"], "keys": "gh<End>X<Esc>"},
    {"name": "s_home_extends", "lines": ["abc def"], "keys": "$gh<Home>X<Esc>"},
    {"name": "s_esc_leaves", "lines": ["abc"], "keys": "gh<Esc>x"},
    {"name": "s_ctrl_a_increments", "lines": ["a1"], "keys": "lgh<C-a>"},
    {"name": "s_dot_repeats_replace", "lines": ["abc def"], "keys": "ghX<Esc>w."},
    {"name": "s_ctrl_j_replaces_with_newline", "lines": ["abc", "def"], "keys": "gh<C-j>X<Esc>"},
    {"name": "s_ctrl_n_extends", "lines": ["abc", "def"], "keys": "gh<C-n>X<Esc>"},
    {"name": "s_ctrl_p_extends", "lines": ["abc", "def"], "keys": "jgh<C-p>X<Esc>"},
    {"name": "s_ctrl_v_to_visual_block", "lines": ["abcd", "efgh"], "keys": "gh<S-Right><S-Down><C-v>X<Esc>"},
    {"name": "s_ctrl_x_decrements", "lines": ["a5"], "keys": "lgh<C-x>"},
    {"name": "s_V_ctrl_g_type", "lines": ["abc", "def", "ghi"], "keys": "Vj<C-g>X<Esc>"},
    {"name": "s_block_type_short_row", "lines": ["abcd", "e", "ijkl"], "keys": "l<C-v>jjl<C-g>X<Esc>"},
    {"name": "s_digit_replaces", "lines": ["abc"], "keys": "gh3<Esc>"},
    {"name": "s_ctrl_o_esc", "lines": ["foo bar"], "keys": "vl<C-g><C-o><Esc>x"},
    {"name": "s_ctrl_o_count", "lines": ["foo bar baz"], "keys": "gh<C-o>2eX<Esc>"},
    {"name": "s_cr_on_last_char", "lines": ["ab"], "keys": "gh<CR><Esc>"},
    # --- #605 page scroll: CTRL-F/CTRL-B, <PageDown>/<PageUp>, <S-Down>/<S-Up>, <S-CR>, and CTRL-D/CTRL-U with
    #     the 'scroll' a count sets. Viewport-dependent, so nvim is aligned to ruse's 80x24 session (a 23-row
    #     window: no status line) and ruse's scrolloff=3. Cursor only; `top` is unit-tested in viewport.rs. --
    {"name": "pg_ctrl_f", "lines": HUNDRED, "keys": "<C-f>", "setup": PAGE_SETUP},
    {"name": "pg_ctrl_f_twice", "lines": HUNDRED, "keys": "<C-f><C-f>", "setup": PAGE_SETUP},
    {"name": "pg_ctrl_f_count", "lines": HUNDRED, "keys": "2<C-f>", "setup": PAGE_SETUP},
    {"name": "pg_ctrl_f_ctrl_b", "lines": HUNDRED, "keys": "<C-f><C-b>", "setup": PAGE_SETUP},
    {"name": "pg_ctrl_b_mid", "lines": HUNDRED, "keys": "50Gzt<C-b>", "setup": PAGE_SETUP},
    {"name": "pg_ctrl_f_mid", "lines": HUNDRED, "keys": "50Gzt<C-f>", "setup": PAGE_SETUP},
    {"name": "pg_ctrl_b_count", "lines": HUNDRED, "keys": "G3<C-b>", "setup": PAGE_SETUP},
    {"name": "pg_ctrl_b_at_top", "lines": HUNDRED, "keys": "5G<C-b>", "setup": PAGE_SETUP},
    {"name": "pg_ctrl_f_keeps_column", "lines": HUNDRED, "keys": "3l<C-f>", "setup": PAGE_SETUP},
    {"name": "pg_ctrl_f_last_page", "lines": HUNDRED, "keys": "G<C-f>", "setup": PAGE_SETUP},
    {"name": "pg_ctrl_f_short_buffer", "lines": ["a", "b", "c"], "keys": "<C-f>", "setup": PAGE_SETUP},
    {"name": "pg_pagedown", "lines": HUNDRED, "keys": "<PageDown>", "setup": PAGE_SETUP},
    {"name": "pg_s_down", "lines": HUNDRED, "keys": "<S-Down>", "setup": PAGE_SETUP},
    {"name": "pg_s_cr", "lines": HUNDRED, "keys": "<S-CR>", "setup": PAGE_SETUP},
    {"name": "pg_pageup", "lines": HUNDRED, "keys": "50Gzt<PageUp>", "setup": PAGE_SETUP},
    {"name": "pg_s_up", "lines": HUNDRED, "keys": "50Gzt<S-Up>", "setup": PAGE_SETUP},
    {"name": "pg_d_ctrl_f_aborts", "lines": HUNDRED, "keys": "5Gd<C-f>", "setup": PAGE_SETUP},
    {"name": "pg_ctrl_d", "lines": HUNDRED, "keys": "<C-d>", "setup": PAGE_SETUP},
    {"name": "pg_ctrl_d_count", "lines": HUNDRED, "keys": "5<C-d>", "setup": PAGE_SETUP},
    {"name": "pg_ctrl_d_count_sticks", "lines": HUNDRED, "keys": "5<C-d><C-d>", "setup": PAGE_SETUP},
    {"name": "pg_ctrl_u", "lines": HUNDRED, "keys": "50G<C-u>", "setup": PAGE_SETUP},
    {"name": "pg_ctrl_u_count_sticks", "lines": HUNDRED, "keys": "50G3<C-u><C-u>", "setup": PAGE_SETUP},
    {"name": "pg_ctrl_u_count_for_ctrl_d", "lines": HUNDRED, "keys": "50G4<C-u><C-d>", "setup": PAGE_SETUP},
    {"name": "pg_ctrl_d_near_end", "lines": HUNDRED, "keys": "90Gzt<C-d>", "setup": PAGE_SETUP},
    {"name": "pg_ctrl_u_at_top", "lines": HUNDRED, "keys": "5G<C-u>", "setup": PAGE_SETUP},
    {"name": "pg_v_ctrl_d", "lines": HUNDRED, "keys": "v<C-d>d", "setup": PAGE_SETUP},
    {"name": "pg_v_ctrl_f", "lines": HUNDRED, "keys": "V<C-f>d", "setup": PAGE_SETUP},
    {"name": "pg_i_pagedown", "lines": HUNDRED, "keys": "lix<PageDown>y<Esc>", "setup": PAGE_SETUP},
    {"name": "pg_i_s_up", "lines": HUNDRED, "keys": "50Gztix<S-Up>y<Esc>", "setup": PAGE_SETUP},
    {"name": "pg_i_pagedown_dot", "lines": HUNDRED, "keys": "ix<PageDown>y<Esc>j.", "setup": PAGE_SETUP},
    # --- #605 modified cursor keys: <S-Left>/<C-Left> = b, <S-Right>/<C-Right> = w, <C-Home> = gg, <C-End> =
    #     line {count} (default last) on its last char; g<Down>/g<Up>/g<Home>/g<End> = gj/gk/g0/g$. -------
    {"name": "n_c_end", "lines": ["  abc def", "xyz", "  last line"], "keys": "<C-End>"},
    {"name": "n_c_end_count", "lines": ["  abc def", "xyz", "  last line"], "keys": "2<C-End>"},
    {"name": "n_c_home", "lines": ["abc def", "xyz", "last line"], "keys": "jj<C-Home>"},
    {"name": "n_c_home_count", "lines": ["abc def", "xyz", "last line"], "keys": "jj2<C-Home>"},
    {"name": "n_d_c_end", "lines": ["abc def", "xyz", "last line"], "keys": "jd<C-End>"},
    {"name": "n_d_c_home", "lines": ["abc def", "xyz", "last line"], "keys": "jd<C-Home>"},
    {"name": "n_s_right", "lines": ["  abc def", "xyz"], "keys": "w<S-Right>"},
    {"name": "n_c_right", "lines": ["  abc def", "xyz"], "keys": "w<C-Right>"},
    {"name": "n_c_left", "lines": ["  abc def", "xyz"], "keys": "w<C-Left>"},
    {"name": "n_s_left", "lines": ["  abc def", "xyz"], "keys": "$<S-Left>"},
    {"name": "n_c_right_wraps", "lines": ["  abc def", "xyz"], "keys": "$<C-Right>"},
    {"name": "n_d_s_right", "lines": ["  abc def", "xyz"], "keys": "d<S-Right>"},
    {"name": "n_d_c_left", "lines": ["  abc def", "xyz"], "keys": "$d<C-Left>"},
    {"name": "n_g_end", "lines": ["  abc def", "xyz"], "keys": "wg<End>"},
    {"name": "n_g_home", "lines": ["  abc def", "xyz"], "keys": "wg<Home>"},
    {"name": "n_g_down", "lines": ["abc def", "xyz"], "keys": "lg<Down>"},
    {"name": "n_g_up", "lines": ["abc def", "xyz"], "keys": "jlg<Up>"},
    {"name": "n_d_g_end", "lines": ["  abc def", "xyz"], "keys": "wdg<End>"},
    {"name": "n_d_g_home", "lines": ["  abc def", "xyz"], "keys": "wdg<Home>"},
    # --- #607 Insert-mode modified keys: <S-Left>/<C-Left> a word back, <S-Right>/<C-Right> a word forward
    #     (across lines), <C-Home> the first char of the file, <C-End> past the last. Caret jumps: the `.`
    #     record restarts after them. -----------------------------------------------------------------------
    {"name": "i_s_left", "lines": ["one two three"], "keys": "wiX<S-Left>Y<Esc>"},
    {"name": "i_c_left", "lines": ["one two three"], "keys": "wiX<C-Left>Y<Esc>"},
    {"name": "i_s_right", "lines": ["one two three"], "keys": "iX<S-Right>Y<Esc>"},
    {"name": "i_c_right", "lines": ["one two three"], "keys": "iX<C-Right>Y<Esc>"},
    {"name": "i_s_right_wraps", "lines": ["one two three", "  four five"], "keys": "$iX<S-Right>Y<Esc>"},
    {"name": "i_s_left_wraps", "lines": ["one two three", "  four five"], "keys": "jiX<S-Left><S-Left>Y<Esc>"},
    {"name": "i_c_right_last_word", "lines": ["xyz", "  last"], "keys": "A<C-Right><C-Right>Y<Esc>"},
    {"name": "i_c_home", "lines": ["  one two", "xyz"], "keys": "jiX<C-Home>Y<Esc>"},
    {"name": "i_c_end", "lines": ["  one two", "xyz", "  last"], "keys": "iX<C-End>Y<Esc>"},
    {"name": "i_s_right_dot", "lines": ["one two three", "  four five"], "keys": "iX<S-Right>Y<Esc>j0."},
    # --- #607 CTRL-G j / k (and <Down>/<Up>, CTRL-J/CTRL-K): a line down / up, to the column where inserting
    #     started (it restarts where typing resumed after a caret jump). CTRL-G U keeps the next <Left>/<Right>
    #     in the undo group and in the `.` record. ------------------------------------------------------------
    {"name": "i_ctrl_g_j", "lines": ["abcdef", "xy", "longer line"], "keys": "lliX<C-g>jY<Esc>"},
    {"name": "i_ctrl_g_j_start_column", "lines": ["abcdef", "xy", "longer line"], "keys": "lliXYZ<C-g>jY<Esc>"},
    {"name": "i_ctrl_g_k", "lines": ["abcdef", "xy", "longer line"], "keys": "jjlliX<C-g>kY<Esc>"},
    {"name": "i_ctrl_g_j_twice", "lines": ["abcdefghijkl", "0123456789", "0123456789abcdef"], "keys": "10liX<C-g>j<C-g>jZ<Esc>"},
    {"name": "i_ctrl_g_down", "lines": ["abcdef", "xy", "longer line"], "keys": "lliX<C-g><Down>Y<Esc>"},
    {"name": "i_ctrl_g_ctrl_j", "lines": ["abcdef", "xy", "longer line"], "keys": "lliX<C-g><C-j>Y<Esc>"},
    {"name": "i_ctrl_g_ctrl_k", "lines": ["abcdef", "xy", "longer line"], "keys": "jjiX<C-g><C-k>Y<Esc>"},
    {"name": "i_ctrl_g_up", "lines": ["abcdef", "xy", "longer line"], "keys": "jjlliX<C-g><Up>Y<Esc>"},
    {"name": "i_ctrl_g_j_last_line", "lines": ["abcdef", "xy"], "keys": "jiX<C-g>jY<Esc>"},
    {"name": "i_ctrl_g_j_dot", "lines": ["abcdef", "xy", "longer line"], "keys": "lliX<C-g>jY<Esc>j."},
    {"name": "i_ctrl_g_j_restarts_after_jump", "lines": ["abcdefghijkl", "0123456789"], "keys": "iX<Right><Right>Y<C-g>jZ<Esc>"},
    {"name": "i_ctrl_g_j_jump_without_typing", "lines": ["abcdefghijkl", "0123456789"], "keys": "iX<Right><Right><C-g>jZ<Esc>"},
    {"name": "i_ctrl_g_j_then_k", "lines": ["abcdefghijkl", "0123456789"], "keys": "lliX<C-g>j<C-g>kZ<Esc>"},
    {"name": "i_ctrl_g_U_left", "lines": ["abcdef", "xy"], "keys": "iX<C-g>U<Left>Y<Esc>"},
    {"name": "i_ctrl_g_U_left_dot", "lines": ["abcdef", "xy"], "keys": "iX<C-g>U<Left>Y<Esc>j0."},
    {"name": "i_ctrl_g_U_right", "lines": ["abcdef", "xy"], "keys": "iX<C-g>U<Right>Y<Esc>"},
    {"name": "i_ctrl_g_U_right_dot", "lines": ["abcdef", "xyzw"], "keys": "iX<C-g>U<Right>Y<Esc>j0."},
    {"name": "i_ctrl_g_U_only_next", "lines": ["abcdef", "xyzw"], "keys": "lliX<C-g>U<Left><Left>Y<Esc>j0."},
    {"name": "i_ctrl_g_U_down_still_jumps", "lines": ["abcdef", "xy", "zzz"], "keys": "iX<C-g>U<Down>Y<Esc>j0."},
    # --- #607 `0 CTRL-D` / `^ CTRL-D`: delete the typed 0/^ and all of the line's indent. -------------------
    {"name": "i_zero_ctrl_d", "lines": ["    foo"], "keys": "A0<C-d>X<Esc>"},
    {"name": "i_zero_ctrl_d_at_start", "lines": ["    foo"], "keys": "I0<C-d>X<Esc>"},
    {"name": "i_caret_ctrl_d", "lines": ["    foo"], "keys": "I^<C-d>X<Esc>"},
    {"name": "i_zero_ctrl_d_after_char", "lines": ["    foo"], "keys": "Ax0<C-d>X<Esc>"},
    {"name": "i_zero_bs_zero_ctrl_d", "lines": ["    foo"], "keys": "A0<BS>0<C-d>X<Esc>"},
    {"name": "i_zero_ctrl_d_tabs", "lines": ["\t \tfoo"], "keys": "A0<C-d>X<Esc>"},
    {"name": "i_zero_ctrl_d_dot", "lines": ["    foo", "  bar"], "keys": "A0<C-d>X<Esc>j."},
    {"name": "i_zero_ctrl_d_no_indent", "lines": ["foo"], "keys": "A0<C-d>X<Esc>"},
    # --- #607 `.` after <Insert> toggled the session between Insert and Replace replays both parts. -------
    {"name": "i_insert_replace_dot", "lines": ["abcdef", "wxyz"], "keys": "iab<Insert>XY<Esc>j0."},
    {"name": "i_insert_replace_insert_dot", "lines": ["abcdef", "wxyz"], "keys": "iab<Insert>XY<Insert>Z<Esc>j0."},
    {"name": "R_insert_toggle_dot", "lines": ["abcdef", "wxyz"], "keys": "RXY<Insert>Z<Esc>j0."},
    {"name": "i_insert_replace_bs", "lines": ["abcdef"], "keys": "iab<Insert>XY<BS><BS><BS><Esc>"},
    #     `!` OVER A SEARCH, `n`, `*`/`#`, `gn` OR A MARK (#800): nvim opens `:.,.+N!` over the lines the
    #     operator span covers (an empty span = `:.!`, no match = E486 and no command line). `tr` is POSIX.
    {"name": "filter_search", "lines": ["a", "b", "x"], "keys": "!/x<CR>tr a-z A-Z<CR>"},
    {"name": "filter_search_mid_line", "lines": ["a", "b", "zx"], "keys": "!/x<CR>tr a-z A-Z<CR>"},
    {"name": "filter_search_backward", "lines": ["a", "b", "x", "b"], "keys": "G!?a<CR>tr a-z A-Z<CR>"},
    {"name": "filter_search_count", "lines": ["a", "b", "x", "x"], "keys": "!2/x<CR>tr a-z A-Z<CR>"},
    {"name": "filter_search_no_match", "lines": ["a", "b", "x"], "keys": "!/zzz<CR>"},
    {"name": "filter_n", "lines": ["foo", "bar", "baz", "foo"], "keys": "/baz<CR>gg!ntr a-z A-Z<CR>"},
    {"name": "filter_gn", "lines": ["foo", "bar", "baz", "foo"], "keys": "/baz<CR>gg!gntr a-z A-Z<CR>"},
    {"name": "filter_mark_line", "lines": ["foo", "bar", "baz", "foo"], "keys": "jjmagg!'atr a-z A-Z<CR>"},
    {"name": "filter_mark_char", "lines": ["foo", "bar", "baz", "foo"], "keys": "jjlmagg!`atr a-z A-Z<CR>"},
    {"name": "filter_mark_char_col0", "lines": ["foo", "bar", "baz", "foo"], "keys": "jjmagg!`atr a-z A-Z<CR>"},
    {"name": "filter_mark_empty", "lines": ["foo bar", "zz"], "keys": "ma!`atr a-z A-Z<CR>"},
    {"name": "filter_star", "lines": ["foo", "bar", "baz", "foo"], "keys": "!*tr a-z A-Z<CR>"},
    {"name": "filter_hash", "lines": ["foo", "bar", "baz", "foo"], "keys": "G!#tr a-z A-Z<CR>"},
    {"name": "filter_g_star", "lines": ["foo", "bar", "baz", "foo"], "keys": "!g*tr a-z A-Z<CR>"},
    {"name": "filter_star_lone_word", "lines": ["foo bar", "zz"], "keys": "!*tr a-z A-Z<CR>"},
    #     `*`/`#`/`g*`/`g#` AS OPERATOR MOTIONS (#1071): an exclusive search for the keyword, run from the
    #     keyword's start, with the span starting at the cursor.
    {"name": "star_under_d", "lines": ["foo bar foo baz"], "keys": "d*"},
    {"name": "star_under_d_mid_word", "lines": ["foo bar foo baz"], "keys": "ld*"},
    {"name": "star_under_d_lone_word", "lines": ["foo bar foo baz"], "keys": "wd*"},
    {"name": "star_under_d_whole_word", "lines": ["foo bar foobar baz"], "keys": "d*"},
    {"name": "hash_under_d", "lines": ["foo bar foo baz"], "keys": "wwd#"},
    {"name": "hash_under_d_wraps_to_self", "lines": ["foo bar foo baz"], "keys": "$hd#"},
    {"name": "g_star_under_d", "lines": ["foo bar foobar baz"], "keys": "dg*"},
    {"name": "star_under_d_op_count", "lines": ["foo bar foo baz foo x"], "keys": "2d*"},
    {"name": "star_under_d_motion_count", "lines": ["foo bar foo baz foo x"], "keys": "d2*"},
    {"name": "star_under_c", "lines": ["foo bar foo baz foo x"], "keys": "c*X<Esc>"},
    {"name": "star_under_y_paste", "lines": ["foo bar foo baz foo x"], "keys": "y*P"},
    {"name": "star_under_gU", "lines": ["foo bar foo baz foo x"], "keys": "gU*"},
    {"name": "star_under_d_dot", "lines": ["foo bar foo baz foo x"], "keys": "d*."},
    {"name": "star_under_d_then_n", "lines": ["foo bar foo baz"], "keys": "d*n"},
    {"name": "star_under_d_multiline", "lines": ["foo bar", "bar foo"], "keys": "d*"},
    {"name": "star_under_d_exclusive_linewise", "lines": ["foo bar", "foo x"], "keys": "d*"},
    {"name": "star_under_shift", "lines": ["foo bar", "foo x"], "keys": ">*", "setup": "set shiftwidth=4 expandtab startofline"},
    {"name": "star_no_keyword", "lines": ["   ", "foo"], "keys": "d*"},
    {"name": "star_count", "lines": ["foo bar foo foo"], "keys": "2*"},
    {"name": "hash_count", "lines": ["foo bar foo foo"], "keys": "$2#"},
    {"name": "star_before_keyword", "lines": ["  foo bar foo"], "keys": "*"},
    {"name": "hash_mid_word", "lines": ["foo bar foo"], "keys": "ll#"},
    #     `gq` AFTER THE EXCLUSIVE-LINEWISE RULE ADJUSTED THE END (nvim `end_adjusted`, #1071): the cursor
    #     lands on the line below the formatted block; `gw` and an inclusive `e` offset do not.
    {"name": "search_under_format_pulled_back", "lines": ["aa bb", "cc", "end", "zz"], "keys": "wgq/end<CR>"},
    {"name": "search_under_format_keep", "lines": ["aa bb", "cc", "end", "zz"], "keys": "gw/end<CR>"},
    {"name": "search_under_format_end_offset", "lines": ["aa bb", "cc", "end", "zz"], "keys": "gq/end/e<CR>"},
    {"name": "search_under_format_last_line", "lines": ["aa bb", "cc", "end"], "keys": "gq/end<CR>"},
    {"name": "n_under_format", "lines": ["aa bb", "cc", "end"], "keys": "/end<CR>gggqn"},
    {"name": "mark_under_format_charwise_col0", "lines": ["aa bb", "cc", "end", "zz"], "keys": "jjmagggq`a"},
    {"name": "paragraph_under_format", "lines": ["aa bb", "cc", "", "zz"], "keys": "gq}"},
    # --- #605 `U`: undo all the latest changes on the last changed line; a second `U` redoes, `.` does not
    #     repeat it. The setup drops the line the oracle's own `nvim_buf_set_lines` saved for `U` (a whole-line
    #     delete clears it, as `dd` does), so each case starts with nothing to restore. -----------------------
    {"name": "U_restores_line", "lines": ["abc def", "ghi jkl", "mno pqr"], "keys": "xxU", "setup": U_SETUP},
    {"name": "U_twice_redoes", "lines": ["abc def", "ghi jkl", "mno pqr"], "keys": "xxUU", "setup": U_SETUP},
    {"name": "U_cursor_column", "lines": ["abc def", "ghi jkl", "mno pqr"], "keys": "wxx0U", "setup": U_SETUP},
    {"name": "U_twice_cursor_column", "lines": ["abc def", "ghi jkl", "mno pqr"], "keys": "wxx0UU", "setup": U_SETUP},
    {"name": "U_after_moving_away", "lines": ["abc def", "ghi jkl", "mno pqr"], "keys": "xxjU", "setup": U_SETUP},
    {"name": "U_last_changed_line_only", "lines": ["abc def", "ghi jkl", "mno pqr"], "keys": "xxjxU", "setup": U_SETUP},
    {"name": "U_back_to_line", "lines": ["abc def", "ghi jkl", "mno pqr"], "keys": "xxjxkUU", "setup": U_SETUP},
    {"name": "U_nothing_to_undo", "lines": ["abc def", "ghi jkl"], "keys": "U", "setup": U_SETUP},
    {"name": "U_count_ignored", "lines": ["abc def", "ghi jkl"], "keys": "xx3U", "setup": U_SETUP},
    {"name": "U_not_repeated_by_dot", "lines": ["abc def", "ghi jkl", "mno pqr"], "keys": "xxU.", "setup": U_SETUP},
    {"name": "U_after_dd_elsewhere", "lines": ["abc def", "ghi jkl", "mno pqr"], "keys": "xxjddU", "setup": U_SETUP},
    {"name": "U_after_2cc", "lines": ["abc def", "ghi jkl", "mno pqr"], "keys": "xxj2ccfoo<Esc>U", "setup": U_SETUP},
    {"name": "U_after_S", "lines": ["abc def", "ghi jkl", "mno pqr"], "keys": "xxjS<Esc>U", "setup": U_SETUP},
    {"name": "U_after_dw", "lines": ["abc def", "ghi jkl", "mno pqr"], "keys": "xxjdwU", "setup": U_SETUP},
    {"name": "U_new_line_from_o", "lines": ["abc def", "ghi jkl", "mno pqr"], "keys": "oxyz<Esc>U", "setup": U_SETUP},
    {"name": "U_o_without_typing", "lines": ["abc def", "ghi jkl", "mno pqr"], "keys": "xxjo<Esc>U", "setup": U_SETUP},
    {"name": "U_O_above", "lines": ["abc def", "ghi jkl", "mno pqr"], "keys": "jxxkO<Esc>U", "setup": U_SETUP},
    {"name": "U_insert_then_cr", "lines": ["abc def", "ghi jkl", "mno pqr"], "keys": "Aab<CR>cd<Esc>U", "setup": U_SETUP},
    {"name": "U_o_then_cr", "lines": ["abc def", "ghi jkl", "mno pqr"], "keys": "oab<CR>cd<Esc>U", "setup": U_SETUP},
    {"name": "U_append", "lines": ["abc def", "ghi jkl", "mno pqr"], "keys": "Axyz<Esc>U", "setup": U_SETUP},
    {"name": "U_insert_after_left", "lines": ["abc def", "ghi jkl", "mno pqr"], "keys": "Aab<Left>cd<Esc>U", "setup": U_SETUP},
    {"name": "U_linewise_put_below", "lines": ["abc def", "ghi jkl", "mno pqr"], "keys": "xxyyjpU", "setup": U_SETUP},
    {"name": "U_gJ_below", "lines": ["abc def", "ghi jkl", "mno pqr"], "keys": "xxjgJU", "setup": U_SETUP},
    {"name": "U_shift_keeps_copy", "lines": ["abc def", "ghi jkl", "mno pqr"], "keys": "jxxk>jU",
     "setup": "set shiftwidth=4 expandtab | " + U_SETUP},
    {"name": "U_after_substitute", "lines": ["abc def", "ghi jkl", "mno pqr"], "keys": "xx:s/c/Q/<CR>U", "setup": U_SETUP},
    {"name": "U_cancels_operator", "lines": ["abc def", "ghi jkl", "mno pqr"], "keys": "xxjdU", "setup": U_SETUP},
    {"name": "U_after_r", "lines": ["abc def", "ghi jkl"], "keys": "jrQlrRkU", "setup": U_SETUP},
    {"name": "U_after_tilde", "lines": ["abc def", "ghi jkl"], "keys": "~~~jU", "setup": U_SETUP},
    {"name": "U_multibyte_column", "lines": ["héllo wörld", "x"], "keys": "wxx0U", "setup": U_SETUP},
    # --- #607 i_CTRL-U: delete the text entered before the caret on this line; with none, back to the first
    #     non-blank, then the indent; at the line start join with the line above ('backspace' "eol"). -----
    {"name": "i_ctrl_u_entered", "lines": ["abc def", "    ghi jkl", "mno"], "keys": "Aab<C-u><Esc>"},
    {"name": "i_ctrl_u_twice", "lines": ["abc def", "    ghi jkl", "mno"], "keys": "Aab<C-u><C-u><Esc>"},
    {"name": "i_ctrl_u_nothing_entered", "lines": ["abc def", "    ghi jkl", "mno"], "keys": "A<C-u><Esc>"},
    {"name": "i_ctrl_u_mid_line", "lines": ["abc def", "    ghi jkl", "mno"], "keys": "wiab<C-u><Esc>"},
    {"name": "i_ctrl_u_mid_line_twice", "lines": ["abc def", "    ghi jkl", "mno"], "keys": "wiab<C-u><C-u><Esc>"},
    {"name": "i_ctrl_u_keeps_indent", "lines": ["abc def", "    ghi jkl", "mno"], "keys": "jAab<C-u><C-u><Esc>"},
    {"name": "i_ctrl_u_indent_then_all", "lines": ["abc def", "    ghi jkl", "mno"], "keys": "jA<C-u><C-u><Esc>"},
    {"name": "i_ctrl_u_at_indent", "lines": ["abc def", "    ghi jkl", "mno"], "keys": "jI<C-u><Esc>"},
    {"name": "i_ctrl_u_typed_at_indent", "lines": ["abc def", "    ghi jkl", "mno"], "keys": "jIx<C-u><C-u><Esc>"},
    {"name": "i_ctrl_u_joins_at_line_start", "lines": ["abc def", "    ghi jkl", "mno"], "keys": "ji<C-u>X<Esc>"},
    {"name": "i_ctrl_u_first_line_start", "lines": ["abc def", "mno"], "keys": "i<C-u>X<Esc>"},
    {"name": "i_ctrl_u_back_across_lines", "lines": ["abc def", "    ghi jkl", "mno"], "keys": "Aab<CR>cd<C-u><C-u><C-u><Esc>"},
    {"name": "i_ctrl_u_after_left", "lines": ["abc def", "    ghi jkl", "mno"], "keys": "Aab<Left><C-u><Esc>"},
    {"name": "i_ctrl_u_dot", "lines": ["abc def", "    ghi jkl", "mno"], "keys": "Aab<C-u>cd<Esc>j."},
    # --- #605 small keys: `z+` / `z^` (the lines below / above the window), `{count}:`, `CTRL-\\ CTRL-N`.
    #     (`q{0-9"}` is unit-tested: the oracle's feedkeys are not typed, so nvim records none of them.) ----
    {"name": "z_plus", "lines": HUNDRED, "keys": "z+", "setup": PAGE_SETUP},
    {"name": "z_plus_twice", "lines": HUNDRED, "keys": "z+z+", "setup": PAGE_SETUP},
    {"name": "z_plus_count", "lines": HUNDRED, "keys": "50z+", "setup": PAGE_SETUP},
    {"name": "z_plus_at_end", "lines": HUNDRED, "keys": "Gz+", "setup": PAGE_SETUP},
    {"name": "z_plus_first_non_blank", "lines": ["  a"] * 40, "keys": "z+", "setup": PAGE_SETUP},
    {"name": "z_caret_at_top", "lines": HUNDRED, "keys": "z^", "setup": PAGE_SETUP},
    {"name": "z_caret", "lines": HUNDRED, "keys": "Gz^", "setup": PAGE_SETUP},
    {"name": "z_caret_count", "lines": HUNDRED, "keys": "50z^", "setup": PAGE_SETUP},
    {"name": "z_caret_count_near_top", "lines": HUNDRED, "keys": "10z^", "setup": PAGE_SETUP},
    {"name": "count_colon_range", "lines": ["a", "b", "c", "d"], "keys": "j2:s/$/!/<CR>"},
    {"name": "count_colon_one", "lines": ["a", "b", "c"], "keys": "j1:d<CR>"},
    {"name": "ctrl_bslash_ctrl_n_normal", "lines": ["abcdefgh", "x"], "keys": "<C-\\><C-n>x"},
    {"name": "ctrl_bslash_ctrl_n_drops_count", "lines": ["abcdefgh", "x"], "keys": "3<C-\\><C-n>x"},
    {"name": "ctrl_bslash_ctrl_n_drops_operator", "lines": ["abcdefgh", "x"], "keys": "d<C-\\><C-n>x"},
    {"name": "ctrl_bslash_ctrl_n_visual", "lines": ["abcdefgh", "x"], "keys": "vl<C-\\><C-n>x"},
    {"name": "ctrl_bslash_ctrl_g_visual", "lines": ["abcdefgh", "x"], "keys": "vl<C-\\><C-g>x"},
    # --- #605 `zp`/`zP`/`zy`: a block's short rows are padded to the block width when text follows them
    #     (`p`/`P`, also between `{count}` copies); `zp`/`zP` add no trailing spaces and `zy` yanks a block
    #     without its trailing white space. -------------------------------------------------------------------
    {"name": "block_p_pads_short_row", "lines": ["abcd", "a", "abcd", "xyz", "xyz", "xyz"], "keys": "l<C-v>jjly3j0p"},
    {"name": "block_P_pads_short_row", "lines": ["abcd", "a", "abcd", "xyz", "xyz", "xyz"], "keys": "l<C-v>jjly3j0P"},
    {"name": "block_p_count_pads_between", "lines": ["abcd", "a", "abcd", "xyz", "xyz", "xyz"], "keys": "l<C-v>jjly3j02p"},
    {"name": "block_p_at_line_end_no_pad", "lines": ["abcd", "a", "abcd"], "keys": "l<C-v>jjly$p"},
    {"name": "block_zp", "lines": ["abcd", "a", "abcd", "xyz", "xyz", "xyz"], "keys": "l<C-v>jjly3j0zp"},
    {"name": "block_zP", "lines": ["abcd", "a", "abcd", "xyz", "xyz", "xyz"], "keys": "l<C-v>jjly3j0zP"},
    {"name": "block_zp_count", "lines": ["abcd", "a", "abcd", "xyz", "xyz", "xyz"], "keys": "l<C-v>jjly3j03zp"},
    {"name": "zp_charwise", "lines": ["abcd"], "keys": "ylzp"},
    {"name": "zp_linewise", "lines": ["abcd", "x"], "keys": "yyjzP"},
    {"name": "block_zy_trims", "lines": ["a b  x", "c d  x", "e    x"], "keys": "l<C-v>jjlllzy"},
    {"name": "block_zy_then_P_keeps_width", "lines": ["a b  x", "c d  x", "e    x", "1234", "1234", "1234"], "keys": "l<C-v>jjlllzy3j0P"},
    {"name": "block_zy_then_zP", "lines": ["a b  x", "c d  x", "e    x", "1234", "1234", "1234"], "keys": "l<C-v>jjlllzy3j0zP"},
    {"name": "block_dollar_P_pads", "lines": ["ab", "abcd", "x", "1234", "1234", "1234"], "keys": "<C-v>jj$y3j0P"},
    {"name": "charwise_zy", "lines": ["a b  x"], "keys": "vllzy"},
    {"name": "normal_zy_motion", "lines": ["abc def"], "keys": "zyw$p"},
    {"name": "normal_zy_count", "lines": ["abc def ghi"], "keys": "2zyw$p"},
]


def generate(path: Path) -> int:
    """Capture every fixture's `expect` from the oracle and write the corpus as JSON-in-YAML."""
    pin = read_pin()
    version_line = nvim_version()
    assert_pin(version_line, pin)

    fixtures = []
    for spec in FIXTURES:
        setup = spec.get("setup", "")
        state = run_neovim(spec["lines"], spec["keys"], setup)
        entry = {
            "name": spec["name"],
            "lines": spec["lines"],
            "keys": spec["keys"],
        }
        # Emit `setup` only for the fixtures that carry one, so the corpus records exactly which runs were
        # config-matched (and stays byte-identical for the config-independent majority).
        if setup:
            entry["setup"] = setup
        fixtures.append(
            {
                **entry,
                "expect": {
                    "text": state["text"],
                    "cursor": state["cursor"],
                    "reg_unnamed": state["reg_unnamed"],
                    "reg0": state["reg0"],
                },
            }
        )

    corpus = {
        "version": 1,
        "generator": "tools/parity/oracle.py",
        "note": (
            "GENERATED — every `expect` was captured from the pinned Neovim oracle, never "
            "hand-written. Regenerate with `python3 tools/parity/oracle.py --generate`. "
            "JSON is a subset of YAML 1.2, so this .yaml parses on both ends without a YAML dep."
        ),
        "oracle": {
            "editor": "neovim",
            "invoke": INVOKE,
            "nvim_version": version_line,
            "pin_version_label": pin["version_label"],
            "pin_revision": pin["revision"],
            "captured_observables": ["text", "cursor", "reg_unnamed", "reg0"],
            "ruse_compare_observables": ["text", "cursor", "reg_unnamed"],
            "reg0_note": (
                "reg0 is captured for completeness but excluded from the ruse comparison: ruse's "
                "core models a single unnamed register (D-026), with no separate yank register."
            ),
        },
        "fixtures": fixtures,
    }
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(corpus, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    print(f"wrote {len(fixtures)} oracle-captured fixtures to {path}")
    print(f"oracle: {version_line} (pin {pin['version_label']} / {pin['revision']})")
    return 0


# --- Selftest: the gate. Every case below has an answer known independently of this harness. ---


def _fail(msg: str) -> None:
    print(f"FAIL: {msg}")


def selftest() -> int:
    """Prove the harness does not corrupt its own observation. Exit non-zero on any disagreement."""
    pin = read_pin()
    version_line = nvim_version()
    print(f"oracle selftest — {version_line} (pin {pin['version_label']})")
    try:
        assert_pin(version_line, pin)
    except OracleError as exc:
        _fail(str(exc))
        return 1

    failures = 0

    # 1. IDENTITY — the empty keystroke must not perturb text or cursor. A harness that mutates on
    #    observation (the core hazard) fails here first.
    ident = run_neovim(["hello", "world"], "")
    if ident["text"] != ["hello", "world"]:
        _fail(f"identity: text changed on keys='' -> {ident['text']}")
        failures += 1
    if ident["cursor"] != [1, 0]:
        _fail(f"identity: cursor moved on keys='' -> {ident['cursor']}")
        failures += 1

    # 2. DETERMINISM — the same (lines, keys) twice must yield identical observations. Non-determinism
    #    means shared state leaked between runs (the shada/config hazard `-u NONE -i NONE` guards).
    a = run_neovim(["foo bar baz"], "dw")
    b = run_neovim(["foo bar baz"], "dw")
    for k in ("text", "cursor", "reg_unnamed", "reg0"):
        if a[k] != b[k]:
            _fail(f"determinism: {k} differs across identical runs: {a[k]} != {b[k]}")
            failures += 1

    # 3. KNOWN OPS — hand-verified expectations. If the oracle disagrees with these, it is LYING and
    #    no fixture recorded through it can be trusted.
    known = [
        ("x on 'hello' -> 'ello'", ["hello"], "x", lambda s: s["text"] == ["ello"]),
        ("dw on 'foo bar' -> 'bar'", ["foo bar"], "dw", lambda s: s["text"] == ["bar"]),
        (
            "dd on ['a','b'] -> ['b']",
            ["a", "b"],
            "dd",
            lambda s: s["text"] == ["b"],
        ),
        (
            "yy sets reg0 linewise",
            ["hello"],
            "yy",
            lambda s: s["reg0"]["type"] == "linewise" and s["reg0"]["text"] == "hello\n",
        ),
    ]
    for label, lines, keys, ok in known:
        state = run_neovim(lines, keys)
        if not ok(state):
            _fail(f"known-op disagreement: {label} — got {state['text']} / reg0={state['reg0']}")
            failures += 1

    if failures:
        print(f"oracle selftest FAILED ({failures} check(s)) — the corpus is NOT trustworthy.")
        return 1
    print("oracle selftest PASSED — identity, determinism, and known ops all hold.")
    return 0


def main(argv: list[str]) -> int:
    args = argv[1:]
    try:
        if "--selftest" in args:
            return selftest()
        if "--generate" in args:
            i = args.index("--generate")
            path = (
                Path(args[i + 1]).resolve()
                if i + 1 < len(args) and not args[i + 1].startswith("-")
                else DEFAULT_CORPUS
            )
            return generate(path)
    except OracleError as exc:
        print(f"oracle error: {exc}")
        return 2
    print(__doc__)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
