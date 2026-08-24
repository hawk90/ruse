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
