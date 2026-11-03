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
       fed with `nvim_feedkeys(nvim_replace_termcodes(keys, true, false, true), 'xt', false)` — mode
       'x' executes synchronously and RETURNS, so every read (`nvim_buf_get_lines`,
       `nvim_win_get_cursor`, `getreg`/`getregtype`) runs against already-settled state. No read
       command is itself an editing command (the `vim -es` / `execute-kbd-macro` trap). 't' feeds
       the keys AS TYPED (#901): untyped keys are flushed after a failed command, never reach a `q`
       recording, and an untyped `<Esc>` runs a command line instead of abandoning it.
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
vim.v.errmsg = ''
local tc = vim.api.nvim_replace_termcodes(input.keys, true, false, true)
if input.probe_failed then
  -- Fed NOT as typed, the keys after a failed command are flushed (Vim's beep_flush and the flush on an
  -- error message), so a sentinel queued after the keys runs only if no command failed. Nothing else of
  -- this run is read.
  _G.ruse_reached = false
  local sentinel = vim.api.nvim_replace_termcodes('<Cmd>lua _G.ruse_reached = true<CR>', true, false, true)
  vim.api.nvim_feedkeys(tc .. sentinel, 'x', false)
  io.write(vim.json.encode({ failed = not _G.ruse_reached }))
  return
end
-- 't': as typed, the way a user's keys arrive, so a failed command does not swallow the keys after it.
vim.api.nvim_feedkeys(tc, 'xt', false)
io.write(vim.json.encode({
  text        = vim.api.nvim_buf_get_lines(0, 0, -1, false),
  cursor      = vim.api.nvim_win_get_cursor(0),
  reg_unnamed = { vim.fn.getreg('"'), vim.fn.getregtype('"') },
  reg0        = { vim.fn.getreg('0'), vim.fn.getregtype('0') },
  mode        = vim.api.nvim_get_mode().mode,
  emsg        = vim.v.errmsg:match('^(E%%d+)') or '',
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


def _nvim(payload: dict) -> dict:
    """One fresh `nvim` process over `payload`; the probe's JSON answer."""
    src = _LUA % json.dumps(payload)
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
            timeout=30,
        )
    except subprocess.TimeoutExpired as exc:
        raise OracleError(f"nvim timed out on keys={payload['keys']!r}") from exc
    finally:
        Path(script).unlink(missing_ok=True)
    if proc.returncode != 0 or not proc.stdout:
        raise OracleError(
            f"nvim failed (rc={proc.returncode}) on keys={payload['keys']!r}: "
            f"{proc.stderr.strip() or '<no stderr>'}"
        )
    # A print command (`:p`, `:z`, #609) writes its lines to stdout ahead of the answer, which the probe
    # writes last, on a line of its own.
    return json.loads(proc.stdout.rsplit("\n", 1)[-1])


def run_neovim(lines: list[str], keys: str, setup: str = "") -> dict:
    """Run the pinned Neovim on `lines`, feed `keys` in Normal mode, and return the settled state.

    Returns {text, cursor:[row1,col0], reg_unnamed:{text,type}, reg0:{text,type}, mode, emsg, failed,
    nvim_version}. The read happens strictly after synchronous feedkeys — see the module docstring for
    why that is the whole point. `setup` is an optional ex command (e.g. `set shiftwidth=4 expandtab`)
    applied to the fresh process before the keys, so a CONFIG-DEPENDENT op can be observed under a
    config that matches ruse's defaults instead of nvim's factory defaults (see the shift_right_line
    fixture).

    The keys are fed AS TYPED, so the keys after a failed command still run, as they do for a user.
    Two error observables (#901) record WHETHER something failed, never the message text: `emsg` is the
    E-number of the last error message the keys raised (`v:errmsg`, cleared first), or ""; `failed` is
    whether any command failed, an error or a Normal-mode beep, observed in a second fresh process as
    Vim flushing the keys queued after the failure (see the probe).
    """
    payload = {"lines": lines, "keys": keys, "setup": setup}
    raw = _nvim(payload)
    failed = _nvim({**payload, "probe_failed": True})["failed"]
    ru_text, ru_type = raw["reg_unnamed"]
    r0_text, r0_type = raw["reg0"]
    return {
        "text": raw["text"],
        "cursor": raw["cursor"],
        "reg_unnamed": {"text": ru_text, "type": _regtype(ru_type)},
        "reg0": {"text": r0_text, "type": _regtype(r0_type)},
        "mode": raw["mode"],
        "emsg": raw["emsg"],
        "failed": failed,
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
# An undo break after the oracle's `nvim_buf_set_lines`, so `u` undoes only the fixture's change.
UNDO_SETUP = "let &undolevels = &undolevels"

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
    {"name": "block_change_column_notation", "lines": ["abc", "def", "ghi"], "keys": "<C-v>jjcX<Esc>"},
    #     ROUND 6 — indent/shift/format (config-dependent), replace-with-newline, quote/tag corners
    {"name": "shift_right_motion_2lines", "lines": ["a", "b", "c"], "keys": ">j", "setup": "set shiftwidth=4 expandtab startofline"},
    {"name": "shift_left_dedents", "lines": ["    ab"], "keys": "<<", "setup": "set shiftwidth=4 expandtab startofline"},
    {"name": "shift_right_paragraph", "lines": ["a", "b", "", "c"], "keys": ">ip", "setup": "set shiftwidth=4 expandtab startofline"},
    {"name": "gq_reflow_paragraph_79", "lines": ["word word word word word word word word word word word word word word word word word word word word"], "keys": "gqip"},
    #     replace char with a newline (splits the line)
    {"name": "replace_char_with_newline", "lines": ["abcdef"], "keys": "llr<CR>"},
    #     quote / tag object corners
    {"name": "ci_squote_adjacent", "lines": ["'a' 'b'"], "keys": "ci'Z<Esc>"},
    {"name": "cit_tag_inner_short", "lines": ["<p>hi</p>"], "keys": "fhcitZ<Esc>"},
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
    #     BLOCKWISE over wide, composing and tabbed text (#909): the block is in screen cells, so a wide
    #     char half inside it, a combining sequence, a ZWJ cluster and a tab each decide the row's slice.
    {"name": "block_d_cjk_rows", "lines": ["가나다", "라마바", "사아자"], "keys": "l<C-v>jd"},
    {"name": "block_d_cjk_over_ascii", "lines": ["abcdef", "가나다", "ghijkl"], "keys": "l<C-v>jjld"},
    {"name": "block_d_ascii_over_cjk", "lines": ["가나다", "abcdef", "라마바"], "keys": "<C-v>jjd"},
    {"name": "block_y_p_cjk", "lines": ["가나다", "라마바"], "keys": "<C-v>jy$p"},
    {"name": "block_d_combining", "lines": ["éái", "xyz"], "keys": "l<C-v>jd"},
    {"name": "block_I_combining", "lines": ["éab", "cde"], "keys": "l<C-v>jIX<Esc>"},
    {"name": "block_d_zwj_cluster", "lines": ["a\U0001F468‍\U0001F469b", "cdefg"], "keys": "l<C-v>jd"},
    {"name": "block_d_tab_row", "lines": ["\tab", "1234cd"], "keys": "<C-v>jd", "setup": "set tabstop=4 noexpandtab"},
    {"name": "block_A_dollar_cjk", "lines": ["가", "abc", "나다"], "keys": "<C-v>jj$AZ<Esc>"},
    {"name": "block_c_cjk", "lines": ["가나다", "라마바"], "keys": "l<C-v>jcZ<Esc>"},
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
    # `&` keeps the last `:s` flags in nvim (`&-default` is `:&&<CR>`), so `g` replaces every match (#700).
    {"name": "icpt_amp_keeps_flags", "lines": ["a a", "a a"], "keys": ":s/a/b/g<CR>j&"},
    {"name": "icpt_amp_count_keeps_flags", "lines": ["a a", "a a", "a a"], "keys": ":s/a/b/g<CR>j2&"},
    # `&` is a mapping to an Ex command, so `.` does not repeat it: it repeats the change before (#700).
    {"name": "icpt_amp_then_dot_no_change", "lines": ["a a", "a a", "a a"], "keys": ":s/a/b/g<CR>j&j."},
    {"name": "icpt_amp_then_dot_repeats_x", "lines": ["xa a", "a a", "a a"], "keys": "x:s/a/b/g<CR>j&j."},
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
    #     cannot continue an armed operator aborts it. Each abort fixture ends at the aborting key: they were
    #     written when the oracle fed keys untyped, which flushes the typeahead on the abort's beep (the
    #     oracle feeds typed keys since #901, so keys after a failure now run, as they do for a user).
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
    # --- #1063 where `u` / `CTRL-R` leave the cursor: Vim's `u_undoredo` puts it back where the change began
    #     (`uh_cursor`) when that is in or next to the re-applied lines, else on the first changed line. The
    #     setup is an undo break, so `u` stops at the oracle's own `nvim_buf_set_lines`; each case makes ONE
    #     change (written when the oracle's keys were untyped and formed a single undo block). -------------
    {"name": "undo_cursor_insert", "lines": ["x", "y"], "keys": "jiab<Esc>u", "setup": UNDO_SETUP},
    {"name": "undo_cursor_insert_ctrl_o", "lines": ["x", "y"], "keys": "jiab<C-o>u", "setup": UNDO_SETUP},
    {"name": "redo_cursor_insert", "lines": ["x", "y"], "keys": "jiab<Esc>u<C-r>", "setup": UNDO_SETUP},
    {"name": "undo_cursor_append_eol", "lines": ["abc def", "ghi"], "keys": "A x<CR>y<Esc>u", "setup": UNDO_SETUP},
    {"name": "undo_cursor_multiline_insert", "lines": ["abc", "def"], "keys": "ix<CR>y<Esc>u", "setup": UNDO_SETUP},
    {"name": "redo_cursor_multiline_insert", "lines": ["abc", "def"], "keys": "ix<CR>y<Esc>u<C-r>", "setup": UNDO_SETUP},
    {"name": "undo_cursor_multiline_append_far", "lines": ["abc", "def"], "keys": "jAx<CR>y<Esc>ggu", "setup": UNDO_SETUP},
    {"name": "undo_cursor_insert_backspace", "lines": ["abc def", "ghi"], "keys": "wi<BS><BS>z<Esc>u", "setup": UNDO_SETUP},
    {"name": "undo_cursor_empty_buffer", "lines": [""], "keys": "ix<Esc>u", "setup": UNDO_SETUP},
    {"name": "redo_cursor_empty_buffer", "lines": [""], "keys": "ix<Esc>u<C-r>", "setup": UNDO_SETUP},
    {"name": "undo_cursor_o_multiline", "lines": ["abc def", "ghi"], "keys": "ofoo<CR>bar<Esc>u", "setup": UNDO_SETUP},
    {"name": "redo_cursor_o_multiline", "lines": ["abc def", "ghi"], "keys": "ofoo<CR>bar<Esc>u<C-r>", "setup": UNDO_SETUP},
    {"name": "undo_cursor_o_keeps_column", "lines": ["abc def", "ghi"], "keys": "wo<Esc>u", "setup": UNDO_SETUP},
    {"name": "redo_cursor_o_keeps_column", "lines": ["abc def", "ghi"], "keys": "wo<Esc>u<C-r>", "setup": UNDO_SETUP},
    {"name": "undo_cursor_O_keeps_column", "lines": ["abc def", "ghi"], "keys": "wOx<Esc>u", "setup": UNDO_SETUP},
    {"name": "redo_cursor_O", "lines": ["abc def", "ghi"], "keys": "wOx<Esc>u<C-r>", "setup": UNDO_SETUP},
    {"name": "redo_cursor_o_last_line_far", "lines": ["a", "b", "c"], "keys": "Goxyz<Esc>ggu<C-r>", "setup": UNDO_SETUP},
    {"name": "undo_cursor_o_empty_line", "lines": ["abc", "", "def"], "keys": "jox<Esc>u", "setup": UNDO_SETUP},
    {"name": "undo_cursor_x_moved_away", "lines": ["abc def", "ghi"], "keys": "wxj0u", "setup": UNDO_SETUP},
    {"name": "redo_cursor_x_moved_away", "lines": ["abc def", "ghi"], "keys": "wx0u$<C-r>", "setup": UNDO_SETUP},
    {"name": "redo_cursor_count_x", "lines": ["abc def", "ghi"], "keys": "w3x0u<C-r>", "setup": UNDO_SETUP},
    {"name": "undo_cursor_X", "lines": ["abc def"], "keys": "$Xu", "setup": UNDO_SETUP},
    {"name": "undo_cursor_db", "lines": ["abc def ghi"], "keys": "wwdbu", "setup": UNDO_SETUP},
    {"name": "redo_cursor_db", "lines": ["abc def ghi"], "keys": "wwdbu<C-r>", "setup": UNDO_SETUP},
    {"name": "undo_cursor_d0", "lines": ["abc def", "ghi"], "keys": "wd0u", "setup": UNDO_SETUP},
    {"name": "undo_cursor_D", "lines": ["abc def"], "keys": "wD0u", "setup": UNDO_SETUP},
    {"name": "undo_cursor_C", "lines": ["abc def"], "keys": "wC<Esc>u", "setup": UNDO_SETUP},
    {"name": "undo_cursor_cw_far", "lines": ["abc def", "ghi", "jkl"], "keys": "jwcwfoo<Esc>ggu", "setup": UNDO_SETUP},
    {"name": "undo_cursor_d_brace", "lines": ["abc", "", "def ghi"], "keys": "Gwd{u", "setup": UNDO_SETUP},
    {"name": "undo_cursor_multibyte", "lines": ["héllo wörld"], "keys": "wxu", "setup": UNDO_SETUP},
    {"name": "redo_cursor_multibyte", "lines": ["héllo wörld"], "keys": "wx0u<C-r>", "setup": UNDO_SETUP},
    {"name": "undo_cursor_gUiw", "lines": ["abc def"], "keys": "wgUiwu", "setup": UNDO_SETUP},
    {"name": "redo_cursor_gUiw", "lines": ["abc def"], "keys": "wgUiw0u<C-r>", "setup": UNDO_SETUP},
    {"name": "undo_cursor_tilde", "lines": ["abc def"], "keys": "w~u", "setup": UNDO_SETUP},
    {"name": "undo_cursor_r", "lines": ["abc def"], "keys": "wrxu", "setup": UNDO_SETUP},
    {"name": "undo_cursor_ctrl_a", "lines": ["x 12", "ghi"], "keys": "<C-a>u", "setup": UNDO_SETUP},
    {"name": "redo_cursor_ctrl_a", "lines": ["x 12", "ghi"], "keys": "<C-a>u<C-r>", "setup": UNDO_SETUP},
    {"name": "undo_cursor_J", "lines": ["abc def", "ghi"], "keys": "wJu", "setup": UNDO_SETUP},
    {"name": "redo_cursor_J", "lines": ["abc def", "ghi"], "keys": "wJu<C-r>", "setup": UNDO_SETUP},
    {"name": "undo_cursor_dd_first", "lines": ["abc def", "ghi"], "keys": "ddu", "setup": UNDO_SETUP},
    {"name": "undo_cursor_dd_keeps_column", "lines": ["abc", "def"], "keys": "llddu", "setup": UNDO_SETUP},
    {"name": "redo_cursor_dd_keeps_column", "lines": ["abc", "def"], "keys": "llddu<C-r>", "setup": UNDO_SETUP},
    {"name": "undo_cursor_dd_last_line", "lines": ["a", "b", "c"], "keys": "Gddu", "setup": UNDO_SETUP},
    {"name": "undo_cursor_dd_last_line_far", "lines": ["a", "b", "c"], "keys": "Gddggu", "setup": UNDO_SETUP},
    {"name": "redo_cursor_dd_last_line", "lines": ["a", "b", "c"], "keys": "Gddu<C-r>", "setup": UNDO_SETUP},
    {"name": "redo_cursor_dd_far", "lines": ["abc def", "ghi", "jkl"], "keys": "jddggu<C-r>", "setup": UNDO_SETUP},
    {"name": "undo_cursor_count_dd_far", "lines": ["ab", "cd", "ef", "gh"], "keys": "jl2ddggu", "setup": UNDO_SETUP},
    {"name": "redo_cursor_count_dd_far", "lines": ["ab", "cd", "ef", "gh"], "keys": "jl2ddggu<C-r>", "setup": UNDO_SETUP},
    {"name": "undo_cursor_dd_empty_line", "lines": ["abc", "", "def"], "keys": "jddu", "setup": UNDO_SETUP},
    {"name": "redo_cursor_dd_empty_line", "lines": ["abc", "", "def"], "keys": "jddu<C-r>", "setup": UNDO_SETUP},
    {"name": "undo_cursor_dk", "lines": ["abc", "def"], "keys": "jlldku", "setup": UNDO_SETUP},
    {"name": "undo_cursor_dk_last_line", "lines": ["abc", "def", "ghi"], "keys": "Gldku", "setup": UNDO_SETUP},
    {"name": "undo_cursor_dj_keeps_column", "lines": ["abc", "def"], "keys": "jlkdju", "setup": UNDO_SETUP},
    {"name": "undo_cursor_shift_keeps_column", "lines": ["abc def", "ghi"], "keys": "ll>>u", "setup": UNDO_SETUP},
    {"name": "undo_cursor_shift_up", "lines": ["abc", "defg", "ghi"], "keys": "jll>ku", "setup": UNDO_SETUP},
    {"name": "undo_cursor_put_line_below", "lines": ["abc def", "ghi"], "keys": "yyjpu", "setup": UNDO_SETUP},
    {"name": "redo_cursor_put_line_below_far", "lines": ["abc def", "ghi"], "keys": "yyjpggu<C-r>", "setup": UNDO_SETUP},
    {"name": "undo_cursor_put_line_above", "lines": ["abc def", "ghi"], "keys": "yyPu", "setup": UNDO_SETUP},
    {"name": "undo_cursor_put_line_above_column", "lines": ["abc def", "ghi"], "keys": "yyllPu", "setup": UNDO_SETUP},
    {"name": "redo_cursor_put_line_above_column", "lines": ["abc def", "ghi"], "keys": "yyllPu<C-r>", "setup": UNDO_SETUP},
    {"name": "undo_cursor_put_line_below_column", "lines": ["abc def", "ghi"], "keys": "yyllpu", "setup": UNDO_SETUP},
    {"name": "undo_cursor_put_line_above_moved", "lines": ["abc def", "  ghi"], "keys": "jyyP2ju", "setup": UNDO_SETUP},
    {"name": "undo_cursor_put_char_after", "lines": ["abc def", "ghi"], "keys": "wylpu", "setup": UNDO_SETUP},
    {"name": "undo_cursor_put_char_before", "lines": ["abc def", "ghi"], "keys": "wylPu", "setup": UNDO_SETUP},
    {"name": "undo_cursor_V_d", "lines": ["abc", "def", "ghi", "jkl"], "keys": "jVjdggu", "setup": UNDO_SETUP},
    {"name": "undo_cursor_V_d_from_below", "lines": ["abc", "def", "ghi"], "keys": "jjlVkdggu", "setup": UNDO_SETUP},
    {"name": "undo_cursor_V_d_anchor_column", "lines": ["abc", "def", "ghi"], "keys": "jlVjdggu", "setup": UNDO_SETUP},
    {"name": "undo_cursor_V_shift", "lines": ["abc", "def", "ghi", "jkl"], "keys": "jlVj>ggu", "setup": UNDO_SETUP},
    {"name": "undo_cursor_V_shift_from_below", "lines": ["abc", "def", "ghi"], "keys": "jjlVk>ggu", "setup": UNDO_SETUP},
    {"name": "undo_cursor_V_U_from_below", "lines": ["abc", "def", "ghi"], "keys": "jjlVkUggu", "setup": UNDO_SETUP},
    {"name": "undo_cursor_v_d", "lines": ["abc", "def", "ghi"], "keys": "jlvjdggu", "setup": UNDO_SETUP},
    {"name": "redo_cursor_v_d", "lines": ["abc", "def", "ghi"], "keys": "jlvjdu<C-r>", "setup": UNDO_SETUP},
    {"name": "undo_cursor_v_U", "lines": ["abc", "def", "ghi"], "keys": "jlvjUggu", "setup": UNDO_SETUP},
    {"name": "undo_cursor_block_d", "lines": ["abcd", "defg", "ghij"], "keys": "jll<C-v>jhdggu", "setup": UNDO_SETUP},
    {"name": "undo_cursor_ex_delete", "lines": ["a", "b", "c"], "keys": ":2,3d<CR>u", "setup": UNDO_SETUP},
    {"name": "undo_cursor_ex_delete_keeps_column", "lines": ["abcdef", "b", "  cxx", "d"], "keys": "4l:3d<CR>u", "setup": UNDO_SETUP},
    {"name": "undo_cursor_ex_delete_first", "lines": ["abc", "def"], "keys": "j:1d<CR>u", "setup": UNDO_SETUP},
    {"name": "undo_cursor_ex_substitute_all", "lines": ["a", "b", "c"], "keys": ":%s/./X/<CR>u", "setup": UNDO_SETUP},
    {"name": "redo_cursor_ex_substitute_all", "lines": ["a", "b", "c"], "keys": ":%s/./X/<CR>uG<C-r>", "setup": UNDO_SETUP},
    {"name": "undo_cursor_ex_substitute_range", "lines": ["a", "  b", "c"], "keys": "G:2,3s/b/X/<CR>u", "setup": UNDO_SETUP},
    {"name": "undo_cursor_ex_substitute_far", "lines": ["abcdef", "b", "  cxx", "d"], "keys": "4l:3s/c/X/<CR>ggu", "setup": UNDO_SETUP},
    # --- #678/#939 undo steps: the keys are fed as typed, so Vim syncs undo before every Normal or Ex command
    #     and each is one step; an Insert session is one step until a cursor key, CTRL-G u or i_CTRL-O
    #     breaks it; the keys of one `@{reg}` (not typed) are one step. ------------------------------------
    {"name": "undo_step_x_x", "lines": ["abc"], "keys": "xxu", "setup": UNDO_SETUP},
    {"name": "undo_step_x_x_uu", "lines": ["abc"], "keys": "xxuu", "setup": UNDO_SETUP},
    {"name": "undo_step_x_x_u_redo", "lines": ["abc"], "keys": "xxu<C-r>", "setup": UNDO_SETUP},
    {"name": "undo_step_x_x_uu_redo", "lines": ["abc"], "keys": "xxuu<C-r>", "setup": UNDO_SETUP},
    {"name": "undo_step_dd_dd", "lines": ["a", "b", "c"], "keys": "ddddu", "setup": UNDO_SETUP},
    {"name": "undo_step_x_dot", "lines": ["abc"], "keys": "x.u", "setup": UNDO_SETUP},
    {"name": "undo_step_count_x_dot", "lines": ["abcdef"], "keys": "3x.u", "setup": UNDO_SETUP},
    {"name": "undo_step_dw_dw", "lines": ["abc def ghi"], "keys": "dwdwu", "setup": UNDO_SETUP},
    {"name": "undo_step_J_J", "lines": ["a b", "c d", "e f"], "keys": "JJu", "setup": UNDO_SETUP},
    {"name": "undo_step_J_dot", "lines": ["a b", "c d", "e f"], "keys": "J.u", "setup": UNDO_SETUP},
    {"name": "undo_step_tilde_tilde", "lines": ["abc"], "keys": "~~u", "setup": UNDO_SETUP},
    {"name": "undo_step_ctrl_a_twice", "lines": ["a 1"], "keys": "<C-a><C-a>u", "setup": UNDO_SETUP},
    {"name": "undo_step_register_x", "lines": ["abc"], "keys": "x\"axu", "setup": UNDO_SETUP},
    {"name": "undo_step_ex_s_then_x", "lines": ["abc"], "keys": ":s/a/q/<CR>xu", "setup": UNDO_SETUP},
    {"name": "undo_step_x_then_ex_s", "lines": ["abc"], "keys": "x:s/b/q/<CR>u", "setup": UNDO_SETUP},
    {"name": "undo_step_ex_s_twice", "lines": ["abc"], "keys": ":s/a/q/<CR>:s/b/r/<CR>u", "setup": UNDO_SETUP},
    {"name": "undo_step_visual_x", "lines": ["abc def"], "keys": "vlxu", "setup": UNDO_SETUP},
    {"name": "undo_step_g_minus_twice", "lines": ["abc def ghi"], "keys": "xwxwxg-g-", "setup": UNDO_SETUP},
    {"name": "undo_step_g_minus_g_plus", "lines": ["abc def ghi"], "keys": "xwxwxg-g-g+", "setup": UNDO_SETUP},
    {"name": "undo_step_insert_one", "lines": ["abc"], "keys": "ofoo<Esc>u", "setup": UNDO_SETUP},
    {"name": "undo_step_cw_insert", "lines": ["abc def"], "keys": "cwfoo<Esc>u", "setup": UNDO_SETUP},
    {"name": "undo_step_replace", "lines": ["abc def"], "keys": "Rxy<BS>z<Esc>u", "setup": UNDO_SETUP},
    {"name": "undo_step_select_type", "lines": ["abc def"], "keys": "ghXY<Esc>u", "setup": UNDO_SETUP},
    {"name": "undo_step_count_insert", "lines": ["abc"], "keys": "3ix<Esc>u", "setup": UNDO_SETUP},
    {"name": "undo_step_insert_dot", "lines": ["abc"], "keys": "ixyz<Esc>.u", "setup": UNDO_SETUP},
    {"name": "undo_step_count_insert_dot", "lines": ["abc def"], "keys": "3ix<Esc>.u", "setup": UNDO_SETUP},
    {"name": "undo_step_insert_count_dot", "lines": ["abc def"], "keys": "ix<Esc>3.u", "setup": UNDO_SETUP},
    {"name": "undo_step_insert_left", "lines": ["abc"], "keys": "iab<Left>cd<Esc>u", "setup": UNDO_SETUP},
    {"name": "undo_step_insert_ctrl_g_u", "lines": ["abc"], "keys": "iab<C-g>ucd<Esc>u", "setup": UNDO_SETUP},
    {"name": "undo_step_insert_ctrl_o_x", "lines": ["abc"], "keys": "iab<C-o>xcd<Esc>u", "setup": UNDO_SETUP},
    {"name": "undo_step_insert_ctrl_o_x_uu", "lines": ["abc"], "keys": "iab<C-o>xcd<Esc>uu", "setup": UNDO_SETUP},
    {"name": "undo_step_insert_ctrl_o_cw", "lines": ["abc def"], "keys": "iab<C-o>cwX<Esc>u", "setup": UNDO_SETUP},
    {"name": "undo_step_insert_ctrl_o_cw_uu", "lines": ["abc def"], "keys": "iab<C-o>cwX<Esc>uu", "setup": UNDO_SETUP},
    {"name": "undo_step_insert_ctrl_o_ex", "lines": ["abc def"], "keys": "iab<C-o>:s/c/Q/<CR>cd<Esc>u", "setup": UNDO_SETUP},
    {"name": "undo_step_insert_ctrl_o_ex_uu", "lines": ["abc def"], "keys": "iab<C-o>:s/c/Q/<CR>cd<Esc>uu", "setup": UNDO_SETUP},
    {"name": "undo_step_normal_one", "lines": ["abc"], "keys": ":normal xx<CR>u", "setup": UNDO_SETUP},
    {"name": "undo_step_global_normal_one", "lines": ["a", "b", "c"], "keys": ":g/./normal x<CR>u", "setup": UNDO_SETUP},
    {"name": "undo_step_macro_record_then_replay", "lines": ["abc"], "keys": "qaxxq@au", "setup": UNDO_SETUP},
    {"name": "undo_step_macro_one", "lines": ["aaaa", "bbbb", "cccc"], "keys": "qaxjxqgg@au", "setup": UNDO_SETUP},
    {"name": "undo_step_macro_count_one", "lines": ["aaaa", "bbbb", "cccc", "dddd"], "keys": "qaxjxqgg2@au", "setup": UNDO_SETUP},
    {"name": "undo_step_macro_at_at", "lines": ["aaaa", "bbbb", "cccc", "dddd"], "keys": "qaxjxqgg@a@@u", "setup": UNDO_SETUP},
    {"name": "undo_step_macro_at_at_uu", "lines": ["aaaa", "bbbb", "cccc", "dddd"], "keys": "qaxjxqgg@a@@uu", "setup": UNDO_SETUP},
    {"name": "undo_step_macro_motion_last", "lines": ["abc def"], "keys": "qaxwq@au", "setup": UNDO_SETUP},
    # #713: `u` puts the cursor on the undone change, far from where it was (fixed by #1063's uh_cursor).
    {"name": "undo_cursor_far_x_then_insert", "lines": ["a", "b", "c"], "keys": "Gxgguiz<Esc>", "setup": UNDO_SETUP},
    # --- #750 `CTRL-R` redoes into the branch last visited (`g-` into the older branch, `u`), not the newest. -
    {"name": "redo_follows_visited_branch", "lines": ["abc"], "keys": "xuAQ<Esc>g-u<C-r>", "setup": UNDO_SETUP},
    {"name": "redo_follows_visited_branch_twice", "lines": ["abc def"], "keys": "xuAQ<Esc>uwxg-g-u<C-r>", "setup": UNDO_SETUP},
    # --- #722 the change list takes an entry per undo step, and none for a change on the newest entry's line
    #     within 79 columns of it; `g;`/`g,` past an end are E662/E663. ------------------------------------
    {"name": "changelist_same_line_one_entry", "lines": ["abcdef", "xyz"], "keys": "xlxjxg;g;g;", "setup": UNDO_SETUP},
    {"name": "changelist_same_line_newest", "lines": ["abcdef", "xyz"], "keys": "xlxjxg;g;", "setup": UNDO_SETUP},
    {"name": "changelist_insert_one_entry", "lines": ["abcdef", "xyz"], "keys": "ihello<Esc>jxg;g;g;0", "setup": UNDO_SETUP},
    {"name": "changelist_far_column_new_entry", "lines": ["a" * 100], "keys": "x$xg;g;", "setup": UNDO_SETUP},
    {"name": "changelist_near_column_no_entry", "lines": ["a" * 100], "keys": "x5lxg;g;", "setup": UNDO_SETUP},
    {"name": "changelist_word_one_entry", "lines": ["abc def"], "keys": "xxwxg;g;g;", "setup": UNDO_SETUP},
    {"name": "changelist_walk_back_and_forth", "lines": ["abcdef", "xyz", "uvw"], "keys": "xjxjxg;g;g;g,g,", "setup": UNDO_SETUP},
    {"name": "changelist_at_start", "lines": ["abc"], "keys": "xg;g;", "setup": UNDO_SETUP},
    {"name": "changelist_at_end", "lines": ["abc"], "keys": "xg;g,", "setup": UNDO_SETUP},
    # --- #725/#940 the jumplist keeps a line once (its newest entry; nvim's cleanup_jumplist); `]]` `[[` `][`
    #     `[]` and a `:s` that matched are jumps. The oracle's own jumplist starts with line 1, so a case
    #     that walks back past its own jumps would see it: these start off line 1 or merge with it. -------
    {"name": "jumplist_line_dedup", "lines": ["a", "b", "c"], "keys": "GggG<C-o><C-o><Tab>", "setup": UNDO_SETUP},
    {"name": "jumplist_line_dedup_walk", "lines": ["a", "b", "c", "d"], "keys": "GggjG<C-o><C-o><C-o>", "setup": UNDO_SETUP},
    {"name": "jumplist_line_dedup_walk_back", "lines": ["a", "b", "c", "d"], "keys": "GggjG<C-o><C-o><C-o><Tab><Tab>", "setup": UNDO_SETUP},
    {"name": "jump_section_fwd", "lines": ["abc", "d", "{", "e", "{", "f"], "keys": "j]]<C-o>", "setup": UNDO_SETUP},
    {"name": "jump_section_back", "lines": ["abc", "d", "{", "e", "{", "f"], "keys": "G[[<C-o>", "setup": UNDO_SETUP},
    {"name": "jump_section_end_fwd", "lines": ["abc", "d", "}", "e", "}", "f"], "keys": "j][<C-o>", "setup": UNDO_SETUP},
    {"name": "jump_section_end_back", "lines": ["abc", "d", "}", "e", "}", "f"], "keys": "G[]<C-o>", "setup": UNDO_SETUP},
    {"name": "jump_ex_substitute", "lines": ["a", "b", "c", "d"], "keys": "jj:2s/b/y/<CR><C-o>", "setup": UNDO_SETUP},
    {"name": "jump_ex_substitute_from_below", "lines": ["a", "b", "c", "d"], "keys": "G:2s/b/y/<CR><C-o>", "setup": UNDO_SETUP},
    {"name": "jump_ex_substitute_no_match", "lines": ["a", "b", "cx", "d"], "keys": "jG:3s/zz/y/<CR>k<C-o>", "setup": UNDO_SETUP},
    # --- #1119 `:d`, `:>`, `:<`, a `:s///c` that matched, and Normal `&`/`g&` that matched are jumps. ---------
    {"name": "jump_ex_delete", "lines": ["a", "b", "c", "d", "e"], "keys": "jj:4d<CR><C-o>", "setup": UNDO_SETUP},
    {"name": "jump_ex_shift_right", "lines": ["a", "b", "c", "d", "e"], "keys": "jj:4><CR><C-o>", "setup": "set shiftwidth=4 expandtab | " + UNDO_SETUP},
    {"name": "jump_ex_shift_left", "lines": ["a", "b", "c", "  d", "e"], "keys": "jj:4<<CR><C-o>", "setup": UNDO_SETUP},
    {"name": "jump_ex_substitute_confirm", "lines": ["a", "b", "c", "d", "e"], "keys": "jj:4s/d/y/c<CR>y<C-o>", "setup": UNDO_SETUP},
    {"name": "jump_ex_substitute_confirm_quit", "lines": ["a", "b", "c", "d", "e"], "keys": "jj:4s/d/y/c<CR>q<C-o>", "setup": UNDO_SETUP},
    {"name": "jump_ex_substitute_confirm_no_match", "lines": ["a", "b", "c", "d", "e"], "keys": "jjG:4s/zz/y/c<CR>k<C-o>", "setup": UNDO_SETUP},
    {"name": "jump_normal_amp", "lines": ["a", "xb", "c", "xd", "xe"], "keys": "jj:s/x/y/<CR>Gk&k<C-o>", "setup": UNDO_SETUP},
    {"name": "jump_normal_amp_last_line", "lines": ["a", "xb", "c", "xd", "xe"], "keys": "jj:s/x/y/<CR>G&k<C-o>", "setup": UNDO_SETUP},
    {"name": "jump_normal_g_amp", "lines": ["a", "xb", "c", "xd", "xe"], "keys": "jj:s/x/y/<CR>kg&<C-o>", "setup": UNDO_SETUP},
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
    #     (`q{0-9"}` is unit-tested; until #901 the oracle's keys were untyped and nvim recorded none.) -----
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
    #     WORD OBJECTS AND THE LINE BREAK (#740): `iw`/`aw` never take a newline as a blank; counts and
    #     repeated Visual objects extend across lines the way nvim's current_word does.
    {"name": "tw_diw_trailing_blanks_eol", "lines": ["foo  ", "  bar"], "keys": "$diw"},
    {"name": "tw_diw_empty_line", "lines": ["a", "", "b"], "keys": "jdiw"},
    {"name": "tw_daw_empty_line", "lines": ["a", "", "b"], "keys": "jdaw"},
    {"name": "tw_daw_last_word_on_line", "lines": ["foo bar", "baz"], "keys": "wdaw"},
    {"name": "tw_daw_only_word_on_line", "lines": ["foo", "bar"], "keys": "daw"},
    {"name": "tw_daw_indented_only_word", "lines": ["  foo", "bar"], "keys": "wdaw"},
    {"name": "tw_yiw_trailing_blanks", "lines": ["foo  ", "bar"], "keys": "$yiw"},
    {"name": "tw_yaw_trailing_blanks", "lines": ["foo  ", "bar"], "keys": "$yaw"},
    {"name": "tw_daw_on_blank_run", "lines": ["foo   bar"], "keys": "4ldaw"},
    {"name": "tw_daw_on_trailing_blank_run", "lines": ["foo bar  ", "baz"], "keys": "$daw"},
    {"name": "tw_d3iw_crosses_line", "lines": ["foo bar", "baz qux"], "keys": "wd3iw"},
    {"name": "tw_d2iw_at_eol", "lines": ["foo bar", "baz qux"], "keys": "wd2iw"},
    {"name": "tw_d2aw_crosses_line", "lines": ["foo bar", "baz qux"], "keys": "wd2aw"},
    {"name": "tw_d3aw_crosses_line", "lines": ["foo bar", "baz qux zz"], "keys": "wd3aw"},
    {"name": "tw_d4iw_over_empty_line", "lines": ["foo", "", "bar baz"], "keys": "d4iw"},
    {"name": "tw_daw_multibyte", "lines": ["한글 abc", "def"], "keys": "daw"},
    {"name": "tw_daw_multibyte_eol", "lines": ["abc 한글", "def"], "keys": "wdaw"},
    {"name": "tw_diw_empty_buffer", "lines": [""], "keys": "diw"},
    {"name": "tw_daw_empty_buffer", "lines": [""], "keys": "daw"},
    {"name": "tw_diw_last_line_unterminated", "lines": ["foo bar"], "keys": "$diw"},
    {"name": "tw_daw_last_word_buffer", "lines": ["foo bar"], "keys": "$daw"},
    {"name": "tw_ciw_trailing_blanks", "lines": ["foo  ", "bar"], "keys": "$ciwX<Esc>"},
    {"name": "tw_v_iw_iw_extends", "lines": ["foo bar baz"], "keys": "viwiwd"},
    {"name": "tw_v_iw_iw_iw_extends", "lines": ["foo bar baz"], "keys": "viwiwiwd"},
    {"name": "tw_v_aw_aw_extends", "lines": ["foo bar baz qux"], "keys": "vawawd"},
    {"name": "tw_v_3iw", "lines": ["foo bar baz"], "keys": "v3iwd"},
    {"name": "tw_v_2aw", "lines": ["foo bar baz qux"], "keys": "v2awd"},
    {"name": "tw_v_iw_across_line", "lines": ["foo bar", "baz qux"], "keys": "wviwiwiwd"},
    {"name": "tw_v_aw_across_line", "lines": ["foo bar", "baz qux"], "keys": "wvawawd"},
    {"name": "tw_v_iw_backward", "lines": ["foo bar baz"], "keys": "$vhiwiwd"},
    {"name": "tw_v_iw_after_motion", "lines": ["foo bar baz"], "keys": "vliwd"},
    {"name": "tw_v_aw_empty_line", "lines": ["a", "", "b"], "keys": "jvawd"},
    {"name": "tw_V_iw_goes_charwise", "lines": ["foo bar", "baz"], "keys": "wViwd"},
    {"name": "tw_yiw_cursor_to_start", "lines": ["foo bar"], "keys": "$yiw"},
    {"name": "tw_daW_punct_word", "lines": ["a.b c.d", "e"], "keys": "wdaW"},
    #     QUOTE OBJECTS AND BACKSLASH ESCAPES (#751): the default 'quoteescape' skips an escaped quote.
    {"name": "tq_di_dquote_escaped", "lines": ["a\"b\\\"c\"d"], "keys": "di\""},
    {"name": "tq_di_dquote_escaped_from_eol", "lines": ["\"a\\\"b\" \"c\""], "keys": "$di\""},
    {"name": "tq_di_dquote_on_escaped_quote", "lines": ["a\"b\\\"c\"d"], "keys": "4ldi\""},
    {"name": "tq_da_dquote_escaped", "lines": ["x \"a\\\"b\" y"], "keys": "da\""},
    {"name": "tq_di_dquote_escaped_backslash", "lines": ["\"a\\\\\" b \"c\""], "keys": "di\""},
    {"name": "tq_di_dquote_multibyte_escaped", "lines": ["x \"한\\\"글\" y"], "keys": "di\""},
    {"name": "tq_di_dquote_between_strings", "lines": ["\"a\" x \"b\""], "keys": "4ldi\""},
    {"name": "tq_di_dquote_on_close", "lines": ["x \"ab\" y"], "keys": "5ldi\""},
    {"name": "tq_di_squote_escaped", "lines": ["'a\\'b' c"], "keys": "di'"},
    {"name": "tq_d2i_dquote", "lines": ["x \"ab\" y"], "keys": "d2i\""},
    {"name": "tq_v_i_dquote_i_dquote", "lines": ["x \"ab\" y"], "keys": "vi\"i\"d"},
    {"name": "tq_v_a_dquote", "lines": ["x \"ab\" y"], "keys": "va\"d"},
    {"name": "tq_di_backtick_escaped", "lines": ["`a\\`b` c"], "keys": "di`"},
    {"name": "tq_v_a_dquote_a_dquote", "lines": ["\"a\" \"b\" c"], "keys": "va\"a\"d"},
    {"name": "tq_ci_dquote_escaped", "lines": ["x \"a\\\"b\" y"], "keys": "ci\"X<Esc>"},
    #     SENTENCE OBJECTS (#752): the gap between sentences, line breaks, paragraph boundaries, counts.
    {"name": "ts_dis_on_gap", "lines": ["Hi. Bye."], "keys": "3ldis"},
    {"name": "ts_das_on_gap", "lines": ["Hi. Bye."], "keys": "3ldas"},
    {"name": "ts_dis_line_break", "lines": ["Hi.", "Bye."], "keys": "dis"},
    {"name": "ts_das_line_break", "lines": ["Hi.", "Bye."], "keys": "das"},
    {"name": "ts_das_last_sentence", "lines": ["Hi there. Bye."], "keys": "$das"},
    {"name": "ts_das_first_sentence", "lines": ["Hi there. Bye."], "keys": "das"},
    {"name": "ts_das_middle", "lines": ["A b. C d. E f."], "keys": "wwdas"},
    {"name": "ts_dis_middle", "lines": ["A b. C d. E f."], "keys": "wwdis"},
    {"name": "ts_dis_two_space_gap", "lines": ["Hi.  Bye."], "keys": "3ldis"},
    {"name": "ts_d2is", "lines": ["A b. C d. E f."], "keys": "d2is"},
    {"name": "ts_d2as", "lines": ["A b. C d. E f."], "keys": "d2as"},
    {"name": "ts_d3is", "lines": ["A b. C d. E f."], "keys": "d3is"},
    {"name": "ts_das_multibyte", "lines": ["가나. 다라. 마바."], "keys": "4ldas"},
    {"name": "ts_das_paragraph_boundary", "lines": ["Hi there", "", "Bye now."], "keys": "das"},
    {"name": "ts_dis_paragraph_boundary", "lines": ["Hi there", "", "Bye now."], "keys": "Gdis"},
    {"name": "ts_dis_empty_line", "lines": ["Hi.", "", "Bye."], "keys": "jdis"},
    {"name": "ts_dis_empty_buffer", "lines": [""], "keys": "dis"},
    {"name": "ts_das_no_terminator", "lines": ["foo bar"], "keys": "wdas"},
    {"name": "ts_das_closing_paren", "lines": ["(Hi.) Bye."], "keys": "das"},
    {"name": "ts_v_is_is", "lines": ["A b. C d. E f."], "keys": "visisd"},
    {"name": "ts_v_as_as", "lines": ["A b. C d. E f. G h."], "keys": "vasasd"},
    {"name": "ts_cis", "lines": ["Hi. Bye."], "keys": "wcisX<Esc>"},
    {"name": "ts_das_after_blank_line", "lines": ["Hi.", "", "Bye."], "keys": "Gdas"},
    {"name": "ts_v_is_on_gap", "lines": ["Hi. Bye."], "keys": "3lvisd"},
    #     BRACKET-OBJECT COUNTS (#753): `2i(` climbs out a level, a block on a later line is found
    #     forward, repeated Visual `i(`/`a(` grow outward.
    {"name": "tp_2di_paren_inner_cursor", "lines": ["((a))"], "keys": "2l2di("},
    {"name": "tp_d2i_paren_inner_cursor", "lines": ["((a))"], "keys": "2ld2i("},
    {"name": "tp_2di_paren_col0", "lines": ["((a))"], "keys": "2di("},
    {"name": "tp_2da_paren", "lines": ["x((a)b)y"], "keys": "3l2da("},
    {"name": "tp_3di_paren_too_deep", "lines": ["((a))"], "keys": "2l3di("},
    {"name": "tp_di_paren_next_line", "lines": ["foo", "(bar)"], "keys": "di("},
    {"name": "tp_di_paren_later_on_line", "lines": ["foo (bar)"], "keys": "di("},
    {"name": "tp_di_paren_on_close", "lines": ["(a(b)c)"], "keys": "4ldi("},
    {"name": "tp_di_paren_on_open", "lines": ["(a(b)c)"], "keys": "2ldi("},
    {"name": "tp_2di_brace_multiline", "lines": ["{", "  x {", "    y", "  }", "}"], "keys": "2j2di{"},
    {"name": "tp_di_paren_multibyte", "lines": ["(한(글)x)"], "keys": "3l2di("},
    {"name": "tp_v_i_paren_i_paren", "lines": ["((a))"], "keys": "2lvi(i(d"},
    {"name": "tp_v_a_paren_a_paren", "lines": ["x((a) b)y"], "keys": "3lva(a(d"},
    {"name": "tp_v_2i_paren", "lines": ["((a))"], "keys": "2lv2i(d"},
    {"name": "tp_v_i_paren_whole_inner", "lines": ["(a(b)c)"], "keys": "3lvi(i(d"},
    {"name": "tp_di_paren_none", "lines": ["foo"], "keys": "di("},
    {"name": "tp_di_bracket_count", "lines": ["[[a]]"], "keys": "2l2di["},
    {"name": "tp_di_paren_empty_inline", "lines": ["x()y"], "keys": "2ldi("},
    {"name": "tp_v_i_paren_grows", "lines": ["((ab))"], "keys": "2lvi(i(d"},
    {"name": "tp_2di_paren_forward", "lines": ["x (a (b))"], "keys": "2di("},
    {"name": "tp_di_paren_escaped", "lines": ["(a \\) b)"], "keys": "ldi("},
    #     INDENTED BLOCKS (#703): a closer preceded only by indent keeps its line; `di{` goes linewise.
    {"name": "tb_di_brace_indented", "lines": ["    if x {", "        foo", "    }"], "keys": "jdi{"},
    {"name": "tb_ci_brace_indented", "lines": ["    if x {", "        foo", "    }"], "keys": "jci{X<Esc>"},
    {"name": "tb_yi_brace_indented", "lines": ["    if x {", "        foo", "    }"], "keys": "jyi{"},
    {"name": "tb_ci_paren_empty_lines", "lines": ["(", ")"], "keys": "ci(X<Esc>"},
    {"name": "tb_di_paren_empty_lines", "lines": ["(", ")"], "keys": "di("},
    {"name": "tb_di_paren_args", "lines": ["f(", "  a,", "  b", "  )"], "keys": "jdi("},
    {"name": "tb_di_brace_tab_indent", "lines": ["{", "\tfoo", "\t}"], "keys": "jdi{"},
    {"name": "tb_di_brace_content_on_open", "lines": ["{ foo", "  bar", "  }"], "keys": "jdi{"},
    {"name": "tb_di_brace_close_trailing", "lines": ["{", "  foo", "  } x"], "keys": "jdi{"},
    {"name": "tb_di_brace_open_trailing_blank", "lines": ["{  ", "foo", "}"], "keys": "jdi{"},
    {"name": "tb_da_brace_indented", "lines": ["    if x {", "        foo", "    }"], "keys": "jda{"},
    {"name": "tb_v_i_brace_indented", "lines": ["    if x {", "        foo", "    }"], "keys": "jvi{d"},
    {"name": "tb_di_brace_close_after_content", "lines": ["{", "  foo }"], "keys": "jdi{"},
    {"name": "tb_di_brace_indented_two_lines", "lines": ["  {", "    a", "    b", "  }", "z"], "keys": "jdi{"},
    {"name": "tb_ci_brace_indented_two_lines", "lines": ["  {", "    a", "    b", "  }", "z"], "keys": "jci{X<Esc>"},
    {"name": "tb_di_brace_cursor_in_indent", "lines": ["    {", "  a", "    }"], "keys": "di{"},
    #     MOTIONS AND THE LINE MODEL (v0.1.3 cluster): `e` over multibyte words (#741), counted `dw` across
    #     line breaks (#742), empty lines as words (#746), `{count}$` and the delete-linewise promotion
    #     (#747), `}` from a blank line (#748), `;` after `t` (#802), linewise delete through an
    #     unterminated last line (#709), and motions/operators at the end of the buffer (#677).
    {"name": "e_multibyte_twice", "lines": ["가나 다라"], "keys": "ee"},
    {"name": "e_multibyte_then_x", "lines": ["가나 다라"], "keys": "eex"},
    #     #909: a base char with combining marks, and a flag, are one character to `e`/`E` and `<BS>`.
    {"name": "e_combining_lands_on_base", "lines": ["éé x"], "keys": "e"},
    {"name": "E_combining_twice", "lines": ["éé áb"], "keys": "EE"},
    {"name": "e_combining_then_x", "lines": ["aé b"], "keys": "ex"},
    {"name": "de_combining", "lines": ["éé x"], "keys": "de"},
    {"name": "insert_bs_combining", "lines": ["éx"], "keys": "$i<BS><Esc>"},
    {"name": "insert_bs_flag", "lines": ["a\U0001F1F0\U0001F1F7x"], "keys": "$i<BS><Esc>"},
    {"name": "insert_bs_zwj", "lines": ["\U0001F468‍\U0001F469x"], "keys": "$i<BS><Esc>"},
    {"name": "esc_after_combining_typed", "lines": ["x"], "keys": "ié<Esc>"},
    {"name": "de_multibyte", "lines": ["가나 다라"], "keys": "lde"},
    {"name": "de_count_punct", "lines": ["foo.bar baz"], "keys": "d2e"},
    {"name": "dw_count_crosses_lines", "lines": ["foo bar", "baz qux"], "keys": "3dw"},
    {"name": "d_count_w_crosses_lines", "lines": ["foo", "bar baz"], "keys": "d2w"},
    {"name": "dw_count_last_word_stops_at_eol", "lines": ["foo bar", "baz"], "keys": "wd3w"},
    {"name": "dw_count_whole_lines_linewise", "lines": ["foo ", "  bar"], "keys": "d2w"},
    {"name": "dw_count_multibyte", "lines": ["가나 다라", "마바 사"], "keys": "3dw"},
    {"name": "w_stops_on_empty_line", "lines": ["foo", "", "bar"], "keys": "wiZ<Esc>"},
    {"name": "W_stops_on_empty_line", "lines": ["foo", "", "bar"], "keys": "WiZ<Esc>"},
    {"name": "b_stops_on_empty_line", "lines": ["foo", "", "bar"], "keys": "GbiZ<Esc>"},
    {"name": "B_stops_on_empty_line", "lines": ["foo", "", "bar"], "keys": "GBiZ<Esc>"},
    {"name": "ge_stops_on_empty_line", "lines": ["foo", "", "bar"], "keys": "GgeiZ<Esc>"},
    {"name": "w_count_over_empty_lines", "lines": ["foo", "", "", "bar"], "keys": "3wiZ<Esc>"},
    {"name": "dw_on_inner_empty_line", "lines": ["foo", "", "bar"], "keys": "jdw"},
    {"name": "d2w_on_empty_lines", "lines": ["foo", "", "", "bar"], "keys": "jd2w"},
    {"name": "cw_on_empty_line", "lines": ["foo", "", "bar"], "keys": "jcwX<Esc>"},
    {"name": "yw_on_empty_line", "lines": ["foo", "", "bar"], "keys": "jywP"},
    {"name": "db_from_col0_linewise", "lines": ["foo", "bar"], "keys": "jdb"},
    {"name": "db_from_col0_charwise", "lines": ["x foo", "bar"], "keys": "jdb"},
    {"name": "count_dollar_move", "lines": ["ab", "cd", "ef"], "keys": "2$x"},
    {"name": "count_dollar_delete_linewise", "lines": ["ab", "cd", "ef"], "keys": "2d$"},
    {"name": "count_dollar_yank_charwise", "lines": ["ab", "cd", "ef"], "keys": "y2$P"},
    {"name": "count_dollar_delete_mid_line", "lines": ["ab", "cd", "ef"], "keys": "l2d$"},
    {"name": "brace_from_blank_skips_blanks", "lines": ["a", "", "", "b", "", "c"], "keys": "j}iZ<Esc>"},
    {"name": "brace_back_from_blank_skips_blanks", "lines": ["a", "", "b", "", "", "c"], "keys": "Gk{iZ<Esc>"},
    {"name": "brace_count_runs_out_fails", "lines": ["a", "b"], "keys": "d2}"},
    {"name": "brace_delete_at_eof", "lines": ["a", "b"], "keys": "d}"},
    {"name": "brace_yank_at_eof", "lines": ["a", "b"], "keys": "y}P"},
    {"name": "brace_change_at_eof", "lines": ["a", "b"], "keys": "c}Z<Esc>"},
    {"name": "brace_delete_at_eof_mid_line", "lines": ["x a", "b"], "keys": "wd}"},
    {"name": "brace_visual_delete_at_eof", "lines": ["a", "b"], "keys": "v}d"},
    {"name": "brace_move_at_eof", "lines": ["a", "bc"], "keys": "}iZ<Esc>"},
    {"name": "semicolon_after_t", "lines": ["axbxcxd"], "keys": "tx;x"},
    {"name": "semicolon_after_t_twice", "lines": ["axbxcxd"], "keys": "tx;;x"},
    {"name": "semicolon_after_T", "lines": ["axbxcxd"], "keys": "$Tx;x"},
    {"name": "semicolon_count_after_t", "lines": ["axbxcxd"], "keys": "2tx;x"},
    {"name": "semicolon_explicit_count_after_t", "lines": ["axbxcxd"], "keys": "tx2;x"},
    {"name": "d_semicolon_after_t", "lines": ["axbxcxd"], "keys": "txd;"},
    {"name": "semicolon_after_f", "lines": ["axbxcxd"], "keys": "fx;x"},
    {"name": "d_forced_linewise_through_last_line", "lines": ["a", "b", "c"], "keys": "jdVj"},
    {"name": "count_dd_on_last_line_fails", "lines": ["a", "b"], "keys": "j2dd"},
    {"name": "d_sentence_at_eof", "lines": ["a", "b"], "keys": "jd)"},
    {"name": "J_last_line_noop", "lines": ["a", "b"], "keys": "GJ"},
    {"name": "J_count_past_end", "lines": ["a", "b"], "keys": "5J"},
    {"name": "J_count_past_end_three", "lines": ["a", "b", "c"], "keys": "5J"},
    {"name": "J_over_empty_line", "lines": ["foo bar", "", "baz qux"], "keys": "3J"},
    {"name": "J_empty_lines_only", "lines": ["a", "", "", "b"], "keys": "3J"},
    {"name": "J_empty_then_word", "lines": ["a", "", "b"], "keys": "3J"},
    {"name": "J_from_empty_line", "lines": ["", "a"], "keys": "J"},
    {"name": "g_underscore_count_past_end", "lines": ["a  ", "b  "], "keys": "5g_iZ<Esc>"},
    {"name": "go_past_end", "lines": ["ab"], "keys": "99goiZ<Esc>"},
    {"name": "w_past_last_word", "lines": ["foo bar"], "keys": "wwiZ<Esc>"},
    {"name": "caret_all_blank_line", "lines": ["   "], "keys": "^iZ<Esc>"},
    #     `(`/`)` through Vim's `findsent` (the `is`/`as` port): an empty line is a sentence of its own.
    {"name": "sentence_fwd_stops_on_blank_line", "lines": ["foo bar", "", "baz qux"], "keys": ")iZ<Esc>"},
    {"name": "sentence_fwd_count_past_blank", "lines": ["foo bar", "", "baz qux"], "keys": "2)iZ<Esc>"},
    {"name": "sentence_fwd_after_terminators", "lines": ["one. two.", "", "three"], "keys": "2)iZ<Esc>"},
    {"name": "sentence_back_to_blank_line", "lines": ["one. two.", "", "three"], "keys": "G(iZ<Esc>"},
    {"name": "sentence_back_count", "lines": ["one. two.", "", "three"], "keys": "G2(iZ<Esc>"},
    {"name": "d_sentence_before_blank_line", "lines": ["foo bar", "", "baz qux"], "keys": "d)"},
    {"name": "d_count_sentence_over_blank", "lines": ["foo bar", "", "baz qux"], "keys": "2d)"},
    {"name": "d_sentence_over_blank_run", "lines": ["a", "", "", "b", "", "", "c"], "keys": "2d)"},
    {"name": "y_sentence_before_blank_paste", "lines": ["foo bar", "", "baz qux"], "keys": "y)P"},
    {"name": "sentence_fwd_closing_paren", "lines": ["x (a.) y. z"], "keys": ")iZ<Esc>"},
    {"name": "sentence_fwd_multibyte", "lines": ["가나. 다라."], "keys": ")iZ<Esc>"},
    #     OPERATOR SPANS (#693, #696, #697, #701, #702, #710, #711, #712, #1077): change over vertical and
    #     linewise spans, `v$`, `a`/`p` on an empty line, the Insert caret after `$`, grapheme clusters,
    #     the exclusive-linewise rule on marks and wrapped searches, forced motions, linewise cursor columns.
    {"name": "os_cj_change_lines", "lines": ["a", "b", "c"], "keys": "cjX<Esc>"},
    {"name": "os_ck_change_lines", "lines": ["a", "b", "c"], "keys": "jckX<Esc>"},
    {"name": "os_cj_indented", "lines": ["  a", "  b", "c"], "keys": "cjX<Esc>"},
    {"name": "os_cj_multibyte", "lines": ["한글", "가", "c"], "keys": "cjX<Esc>"},
    {"name": "os_cj_last_line_fails", "lines": ["a", "b"], "keys": "Gcj"},
    {"name": "os_cG_to_last_line", "lines": ["a", "b", "c"], "keys": "jcGX<Esc>"},
    {"name": "os_c_para_keeps_blank", "lines": ["a", "b", "", "c"], "keys": "c}X<Esc>"},
    {"name": "os_c_para_back", "lines": ["a", "", "b", "c"], "keys": "Gc{X<Esc>"},
    {"name": "os_c_mark_line", "lines": ["a", "b", "c"], "keys": "majc'aX<Esc>"},
    {"name": "os_c_forced_linewise", "lines": ["a", "b", "c"], "keys": "cVjX<Esc>"},
    {"name": "os_3cc_last_line_fails", "lines": ["a", "b"], "keys": "j3cc"},
    {"name": "os_c3w_one_line", "lines": ["a b"], "keys": "c3wX<Esc>"},
    {"name": "os_v_dollar_d", "lines": ["abc", "def"], "keys": "v$d"},
    {"name": "os_v_dollar_c", "lines": ["abc", "def"], "keys": "v$cX<Esc>"},
    {"name": "os_v_dollar_yP", "lines": ["abc", "def"], "keys": "v$yP"},
    {"name": "os_v_dollar_last_line", "lines": ["abc", "def"], "keys": "jv$y"},
    {"name": "os_v_dollar_j_d", "lines": ["abc", "def", "ghi"], "keys": "v$jd"},
    {"name": "os_v_dollar_empty_line", "lines": ["", "def"], "keys": "v$d"},
    {"name": "os_v_dollar_multibyte", "lines": ["한글", "def"], "keys": "v$d"},
    {"name": "os_a_empty_line", "lines": ["", "abc"], "keys": "aX<Esc>"},
    {"name": "os_a_empty_buffer", "lines": [""], "keys": "aX<Esc>"},
    {"name": "os_p_empty_line", "lines": ["q", "", "abc"], "keys": "yljp"},
    {"name": "os_ctrl_o_p_eol", "lines": ["foo bar"], "keys": "yiwA<C-o>pX<Esc>"},
    {"name": "os_dollar_cb", "lines": ["foo bar"], "keys": "$cbX<Esc>"},
    {"name": "os_dollar_ch", "lines": ["abc"], "keys": "$chX<Esc>"},
    {"name": "os_dollar_c0", "lines": ["abc"], "keys": "$c0X<Esc>"},
    {"name": "os_dollar_gi", "lines": ["abc def"], "keys": "iQ<Esc>$giX<Esc>"},
    {"name": "os_ctrl_o_mark_mid_line", "lines": ["foo bar"], "keys": "4lma0A<C-o>`aX<Esc>"},
    {"name": "os_ctrl_o_dd_appends", "lines": ["abc", "de"], "keys": "A<C-o>ddX<Esc>"},
    {"name": "os_ctrl_o_dollar_appends", "lines": ["abc def"], "keys": "i<C-o>$X<Esc>"},
    {"name": "os_x_combining", "lines": ["éx"], "keys": "x"},
    {"name": "os_x_zwj", "lines": ["\U0001F469‍\U0001F4BBx"], "keys": "x"},
    {"name": "os_lx_zwj", "lines": ["\U0001F469‍\U0001F4BBx"], "keys": "lx"},
    {"name": "os_r_combining", "lines": ["éb"], "keys": "rZ"},
    {"name": "os_s_combining", "lines": ["éb"], "keys": "sZ<Esc>"},
    {"name": "os_dl_combining", "lines": ["éb"], "keys": "dl"},
    {"name": "os_lx_combining_eol", "lines": ["éb"], "keys": "lx"},
    {"name": "os_2x_combining", "lines": ["ééb"], "keys": "2x"},
    {"name": "os_dollar_x_combining", "lines": ["abé"], "keys": "$x"},
    {"name": "os_d_backtick_col0", "lines": ["ab", "cd", "ef"], "keys": "jmakld`a"},
    {"name": "os_d_backtick_col0_indent", "lines": ["  ab", "  cd", "ef"], "keys": "jjmaggd`a"},
    {"name": "os_d_backtick_from_indent", "lines": ["  ab", "cd", "ef"], "keys": "jmakd`a"},
    {"name": "os_c_backtick_col0", "lines": ["ab", "cd", "ef"], "keys": "jmaklc`aZ<Esc>"},
    {"name": "os_y_backtick_col0", "lines": ["ab", "cd", "ef"], "keys": "jmakly`a"},
    {"name": "os_d_search_wrapped", "lines": ["foo", "x", "bar"], "keys": "Gd/foo<CR>"},
    {"name": "os_d_search_wrapped_mid", "lines": ["a foo", "x", "bar"], "keys": "Gld/foo<CR>"},
    {"name": "os_dv_g_", "lines": ["c  "], "keys": "dvg_"},
    {"name": "os_dv_g_word", "lines": ["abc  "], "keys": "dvg_"},
    {"name": "os_dv_ge", "lines": ["for"], "keys": "$dvge"},
    {"name": "os_dv_ge_two_lines", "lines": ["abc", "def"], "keys": "$dvge"},
    {"name": "os_dv_percent", "lines": ["()"], "keys": "dv%"},
    {"name": "os_dvG_keeps_col", "lines": ["ab", "cd"], "keys": "ldvG"},
    {"name": "os_dv2G_keeps_col", "lines": ["ab", "cd", "ef"], "keys": "ldv2G"},
    {"name": "os_dvgg_keeps_col", "lines": ["ab", "cd"], "keys": "jldvgg"},
    {"name": "os_dVw_last_word", "lines": ["abc", "def"], "keys": "dVw"},
    {"name": "os_dvb", "lines": ["abc def"], "keys": "wdvb"},
    {"name": "os_dvt", "lines": ["abc def"], "keys": "dvtd"},
    {"name": "os_yy_keeps_col", "lines": ["abc def"], "keys": "wyyiZ<Esc>"},
    {"name": "os_yj_keeps_col", "lines": ["abc def", "ghi"], "keys": "wyjiZ<Esc>"},
    {"name": "os_yk_keeps_col", "lines": ["abc def", "ghi jkl"], "keys": "jwykiZ<Esc>"},
    {"name": "os_gUj_keeps_col", "lines": ["abc def", "ghi"], "keys": "wgUjiZ<Esc>"},
    {"name": "os_gUk_keeps_col", "lines": ["abc def", "ghi jkl"], "keys": "jwgUkiZ<Esc>"},
    {"name": "os_gUU_line_start", "lines": ["abc def", "ghi jkl"], "keys": "wgUUiZ<Esc>"},
    {"name": "os_3yy_last_line_fails", "lines": ["a", "b"], "keys": "j3yy"},
    {"name": "os_3dd_last_line_fails", "lines": ["a", "b"], "keys": "j3dd"},
    {"name": "os_Vd_keeps_col", "lines": ["abc def", "ghi jkl", "mno"], "keys": "wVd"},
    {"name": "os_Vjd_keeps_col", "lines": ["abc def", "ghi jkl", "mno pqr"], "keys": "wVjd"},
    {"name": "os_Vd_then_P", "lines": ["PP", "abc", "xyz"], "keys": "yiwjllVdP"},
    {"name": "os_dd_keeps_col_in_indent", "lines": ["  x", "  abc", "  def"], "keys": "ldd"},
    {"name": "os_dd_after_dollar", "lines": ["abc def", "ghi"], "keys": "w$ddiZ<Esc>"},
    {"name": "os_dd_short_next_line", "lines": ["ab", "  cd", "ef"], "keys": "jllldd"},
    {"name": "os_dj_keeps_col", "lines": ["abc def", "ghi jkl", "xyz"], "keys": "wdjiZ<Esc>"},
    {"name": "os_G_keeps_col", "lines": ["ab", "cd"], "keys": "lG"},
    {"name": "os_G_keeps_col_indent", "lines": ["ab", "  cd"], "keys": "lG"},
    {"name": "os_gg_keeps_col", "lines": ["ab", "cd"], "keys": "jlgg"},
    {"name": "os_G_after_dollar", "lines": ["abc", "de"], "keys": "$G"},
    #     BLOCKWISE OPS IN SCREEN COLUMNS (#694, #695, #1064, #1080): one geometry for every `CTRL-V` op.
    #     Corners are placed with `l`/`0`/`f` (not `j`/`k` across a `<Tab>`, whose column is the curswant
    #     gap) so ruse and nvim select the same block. ts=4 matches ruse's default tabstop.
    {"name": "blk_U_rows", "lines": ["abc", "def"], "keys": "<C-v>jU"},
    {"name": "blk_u_rows", "lines": ["ABC", "DEF"], "keys": "<C-v>ju"},
    {"name": "blk_tilde_rows", "lines": ["abc", "def"], "keys": "<C-v>j~"},
    {"name": "blk_rot13_rows", "lines": ["abc", "def"], "keys": "<C-v>jg?"},
    {"name": "blk_r_rows", "lines": ["abc", "def"], "keys": "<C-v>jrx"},
    {"name": "blk_r_short_row", "lines": ["abcd", "a", "abcd"], "keys": "l<C-v>jjlrx"},
    {"name": "blk_U_dollar", "lines": ["ab", "abcd"], "keys": "<C-v>j$U"},
    {"name": "blk_paste_charwise_reg", "lines": ["abc", "def", "ghi", "ZZ"], "keys": "Gyiwgg<C-v>jp"},
    {"name": "blk_paste_past_eof", "lines": ["abcd", "efgh", "XY", "ZW"], "keys": "gg<C-v>jlyGp"},
    {"name": "blk_paste_pads_short_row", "lines": ["abc", "a", "abc", "xyz", "xyz", "xyz"], "keys": "l<C-v>jjlyG0kklP"},
    {"name": "blk_paste_into_end_slot", "lines": ["ab", "cd"], "keys": "<C-v>jyGo<Esc>p"},
    {"name": "blk_paste_issue_695_short_rows", "lines": ["abc", "a", "abc", "xyz", "xyz", "xyz"], "keys": "l<C-v>jjlyGkkP"},
    {"name": "vput_issue_704_char_sel_block_reg", "lines": ["abc", "def", "ghi"], "keys": "ll<C-v>jyggvp"},
    {"name": "vput_issue_704_block_same_block", "lines": ["abc", "def", "ghi"], "keys": "ll<C-v>jygg<C-v>jp"},
    {"name": "blk_paste_count_past_eof", "lines": ["abcd", "efgh", "x"], "keys": "l<C-v>jlyG2p"},
    {"name": "blk_zp_no_fill", "lines": ["abcd", "efgh", "xy"], "keys": "l<C-v>jlyG0lzp"},
    {"name": "blk_paste_dollar_pads", "lines": ["ab", "abcd", "x"], "keys": "<C-v>j$yGPx"},
    {"name": "blk_yank_short_line_spaces", "lines": ["abcdef", "a", "abcdef"], "keys": "fc<C-v>jj0fdy"},
    {"name": "blk_yank_short_line_dollar", "lines": ["abcdef", "a", "abcdef"], "keys": "fc<C-v>jj0fd$y"},
    {"name": "blk_tab_corner_d", "lines": ["a\tbc", "abcdefgh"], "keys": "l<C-v>j0fdd", "setup": "set ts=4"},
    {"name": "blk_tab_corner_y", "lines": ["a\tbc", "abcdefgh"], "keys": "l<C-v>j0fdy", "setup": "set ts=4"},
    {"name": "blk_tab_corner_U", "lines": ["a\tbc", "abcdefgh"], "keys": "l<C-v>j0fdU", "setup": "set ts=4"},
    {"name": "blk_tab_corner_r", "lines": ["a\tbc", "abcdefgh"], "keys": "l<C-v>j0fdrx", "setup": "set ts=4"},
    {"name": "blk_tab_corner_I", "lines": ["a\tbc", "abcdefgh"], "keys": "l<C-v>j0fdIX<Esc>", "setup": "set ts=4"},
    {"name": "blk_tab_corner_A", "lines": ["a\tbc", "abcdefgh"], "keys": "l<C-v>j0fdAX<Esc>", "setup": "set ts=4"},
    {"name": "blk_tab_corner_c", "lines": ["a\tbc", "abcdefgh"], "keys": "l<C-v>j0fdcX<Esc>", "setup": "set ts=4"},
    {"name": "blk_tab_split_right_y", "lines": ["a\tbc", "abcdefgh"], "keys": "<C-v>jfby", "setup": "set ts=4"},
    {"name": "blk_tab_split_right_d", "lines": ["a\tbc", "abcdefgh"], "keys": "<C-v>jfbd", "setup": "set ts=4"},
    {"name": "blk_tab_split_right_r", "lines": ["a\tbc", "abcdefgh"], "keys": "<C-v>jfbrx", "setup": "set ts=4"},
    {"name": "blk_tab_split_right_c", "lines": ["a\tbc", "abcdefgh"], "keys": "<C-v>jfbcX<Esc>", "setup": "set ts=4"},
    {"name": "blk_tab_split_left_y", "lines": ["abcdefgh", "a\tbc"], "keys": "fc<C-v>j0fby", "setup": "set ts=4"},
    {"name": "blk_tab_split_left_d", "lines": ["abcdefgh", "a\tbc"], "keys": "fc<C-v>j0fbd", "setup": "set ts=4"},
    {"name": "blk_tab_split_left_I", "lines": ["abcdefgh", "a\tbc"], "keys": "fc<C-v>j0fbIX<Esc>", "setup": "set ts=4"},
    {"name": "blk_tab_split_left_A", "lines": ["abcdefgh", "a\tbc"], "keys": "fc<C-v>j0fbAX<Esc>", "setup": "set ts=4"},
    {"name": "blk_tab_split_left_r", "lines": ["abcdefgh", "a\tbc"], "keys": "fc<C-v>j0fbrx", "setup": "set ts=4"},
    {"name": "blk_tab_split_left_U", "lines": ["abcdefgh", "a\tbc"], "keys": "fc<C-v>j0fbU", "setup": "set ts=4"},
    {"name": "blk_tab_split_I_lower", "lines": ["abcdefgh", "a\tbc"], "keys": "fb<C-v>jIX<Esc>", "setup": "set ts=4"},
    {"name": "blk_tab_split_A_lower", "lines": ["abcdefgh", "a\tbc"], "keys": "fb<C-v>jAX<Esc>", "setup": "set ts=4"},
    {"name": "blk_paste_after_tab", "lines": ["a\tbc", "abcdefgh"], "keys": "jl<C-v>lykp", "setup": "set ts=4"},
    {"name": "blk_paste_before_tab", "lines": ["a\tbc", "abcdefgh"], "keys": "jl<C-v>lykP", "setup": "set ts=4"},
    {"name": "blk_paste_splits_tab", "lines": ["xy", "xy", "abc", "a\tb"], "keys": "<C-v>jlyjjlp", "setup": "set ts=4"},
    {"name": "blk_paste_splits_tab_P", "lines": ["xy", "xy", "abc", "a\tb"], "keys": "<C-v>jlyjjlP", "setup": "set ts=4"},
    {"name": "blk_paste_wide_straddle", "lines": ["xy", "xy", "abc", "a宽b"], "keys": "<C-v>jlyjjlp"},
    {"name": "blk_paste_wide_straddle_P", "lines": ["xy", "xy", "abc", "a宽b"], "keys": "<C-v>jlyjjlP"},
    {"name": "blk_wide_corner_d", "lines": ["a宽bc", "abcdefgh"], "keys": "f宽<C-v>jfcd"},
    {"name": "blk_wide_corner_y", "lines": ["a宽bc", "abcdefgh"], "keys": "f宽<C-v>jfcy"},
    {"name": "blk_wide_corner_U", "lines": ["a宽bc", "abcdefgh"], "keys": "f宽<C-v>jfcU"},
    {"name": "blk_wide_corner_r", "lines": ["a宽bc", "abcdefgh"], "keys": "f宽<C-v>jfcrx"},
    {"name": "blk_wide_corner_I", "lines": ["a宽bc", "abcdefgh"], "keys": "f宽<C-v>jfcIX<Esc>"},
    {"name": "blk_wide_corner_A", "lines": ["a宽bc", "abcdefgh"], "keys": "f宽<C-v>jfcAX<Esc>"},
    {"name": "blk_wide_split_d", "lines": ["a宽bc", "abcdefgh"], "keys": "<C-v>jfbd"},
    {"name": "blk_r_wide_char_odd", "lines": ["a宽bc", "abcdefgh"], "keys": "<C-v>lr宽"},
    {"name": "blk_r_wide_char_even", "lines": ["abc", "abcdefgh"], "keys": "<C-v>jlr宽"},
    {"name": "vrep_tab_charwise", "lines": ["\tabcdef", "abcdefgh"], "keys": "vldj0.", "setup": "set ts=4"},
    {"name": "vrep_tab_block", "lines": ["\tab", "\tab", "abcdefgh", "abcdefgh"], "keys": "<C-v>jdjj0.", "setup": "set ts=4"},
    {"name": "vrep_tab_block_r", "lines": ["\tab", "\tab", "abcdefgh", "abcdefgh"], "keys": "<C-v>jrxjj0.", "setup": "set ts=4"},
    {"name": "vrep_wide_charwise", "lines": ["宽宽x", "abcdefgh"], "keys": "vldj0."},
    {"name": "vrep_wide_block", "lines": ["宽ab", "宽ab", "abcdefgh", "abcdefgh"], "keys": "<C-v>jdjj0."},
    {"name": "vrep_wide_block_U", "lines": ["宽ab", "宽ab", "abcdefgh", "abcdefgh"], "keys": "<C-v>jUjj0."},
    #     VISUAL PUT BY SELECTION x REGISTER KIND (#704, nvim `nv_put_opt`).
    {"name": "vput_line_sel_char_reg", "lines": ["a", "b", "c"], "keys": "yiwjVp"},
    {"name": "vput_line_sel_char_reg_P", "lines": ["a", "b", "c"], "keys": "yiwjVP"},
    {"name": "vput_line_sel_char_reg_last", "lines": ["abc", "def", "ghi"], "keys": "yiwGVp"},
    {"name": "vput_line_sel_two_lines", "lines": ["ab", "cd", "ef"], "keys": "yljVjp"},
    {"name": "vput_line_sel_line_reg", "lines": ["abc", "def", "ghi"], "keys": "yyjVp"},
    {"name": "vput_line_sel_line_reg_last", "lines": ["abc", "def", "ghi"], "keys": "yyGVp"},
    {"name": "vput_line_sel_block_reg", "lines": ["abc", "def", "ghi"], "keys": "ll<C-v>jyjVp"},
    {"name": "vput_line_sel_block_reg_last", "lines": ["abc", "def", "ghi"], "keys": "ll<C-v>jyGVp"},
    {"name": "vput_char_sel_line_reg", "lines": ["foo bar", "x"], "keys": "jyykviwp"},
    {"name": "vput_char_sel_line_reg_P", "lines": ["foo bar", "x"], "keys": "jyykviwP"},
    {"name": "vput_char_sel_line_reg_eol", "lines": ["foo bar", "baz"], "keys": "jyyk$viwp"},
    {"name": "vput_char_sel_lines_reg", "lines": ["foo bar", "x", "y"], "keys": "jVjyggviwp"},
    {"name": "vput_char_sel_line_reg_mid", "lines": ["abc", "def", "ghi"], "keys": "yyjvlp"},
    {"name": "vput_char_sel_line_reg_end", "lines": ["abc", "def", "ghi"], "keys": "yyj$vp"},
    {"name": "vput_char_sel_block_reg", "lines": ["abc", "def", "ghi"], "keys": "ll<C-v>jyG$vp"},
    {"name": "vput_char_sel_multiline_char_reg", "lines": ["abc", "def"], "keys": "ylvjp"},
    {"name": "vput_char_sel_lines_line_reg", "lines": ["abc", "def"], "keys": "yyvjp"},
    {"name": "vput_char_sel_last_word", "lines": ["abc", "def", "ghi"], "keys": "yiwGv$p"},
    {"name": "vput_char_sel_named_reg", "lines": ["abc", "def", "ghi"], "keys": "\"ayljvl\"ap"},
    {"name": "vput_block_sel_char_reg", "lines": ["abc", "def", "ghi"], "keys": "yl<C-v>jlp"},
    {"name": "vput_block_sel_char_reg_P", "lines": ["abc", "def", "ghi"], "keys": "yl<C-v>jlP"},
    {"name": "vput_block_sel_char_reg_dollar", "lines": ["abc", "def", "ghi"], "keys": "yl<C-v>jj$p"},
    {"name": "vput_block_sel_char_reg_short", "lines": ["abc", "de", "ghi"], "keys": "yl<C-v>jjllp"},
    {"name": "vput_block_sel_char_reg_at_eol", "lines": ["abcd", "abcd", "abcd"], "keys": "ylfd<C-v>jjp"},
    {"name": "vput_block_sel_char_reg_mid", "lines": ["abcd", "abcd", "abcd"], "keys": "ylfc<C-v>jjp"},
    {"name": "vput_block_sel_line_reg", "lines": ["abc", "def", "ghi"], "keys": "yy<C-v>jlp"},
    {"name": "vput_block_sel_multiline_char_reg", "lines": ["abc", "def", "ghi"], "keys": "vjy<C-v>jlp"},
    {"name": "vput_block_sel_block_reg", "lines": ["abcd", "efgh", "ijkl"], "keys": "l<C-v>jly<C-v>p"},
    {"name": "vput_block_sel_one_cell", "lines": ["ab", "cd"], "keys": "ylj<C-v>p"},
    {"name": "vput_block_sel_block_reg_wider", "lines": ["abc", "def"], "keys": "vly<C-v>jp"},
    #     EX RANGES (#1079): `.`/`$`/offsets/`;`, E16 past the last line, and the cursor after the command.
    {"name": "exr_d_past_end", "lines": ["a", "b", "c"], "keys": ":2,6d<CR>"},
    {"name": "exr_d_line_past_end", "lines": ["a", "b", "c"], "keys": ":5d<CR>"},
    {"name": "exr_y_past_end", "lines": ["a", "b", "c"], "keys": ":2,6y<CR>"},
    {"name": "exr_s_past_end", "lines": ["a", "b", "c"], "keys": ":2,6s/./X/<CR>"},
    {"name": "exr_m_past_end", "lines": ["a", "b", "c"], "keys": ":2,6m0<CR>"},
    {"name": "exr_t_past_end", "lines": ["a", "b", "c"], "keys": ":2,6t0<CR>"},
    {"name": "exr_j_past_end", "lines": ["a", "b", "c"], "keys": ":2,6j<CR>"},
    {"name": "exr_normal_past_end", "lines": ["a", "b", "c"], "keys": ":2,6normal Ax<CR>"},
    {"name": "exr_goto_past_end", "lines": ["a", "b", "c"], "keys": ":5<CR>"},
    {"name": "exr_d_zero", "lines": ["a", "b", "c"], "keys": ":0d<CR>"},
    {"name": "exr_d_to_dollar", "lines": ["abc", "def", "ghi"], "keys": ":2,$d<CR>"},
    {"name": "exr_d_dot_dollar", "lines": ["abc", "def", "ghi"], "keys": "j:.,$d<CR>"},
    {"name": "exr_d_dot_offset", "lines": ["abc", "def", "ghi", "jkl"], "keys": ":.,.+1d<CR>"},
    {"name": "exr_d_offsets", "lines": ["abc", "def", "ghi", "jkl"], "keys": ":.+1,$-1d<CR>"},
    {"name": "exr_d_before_start", "lines": ["abc", "def", "ghi", "jkl"], "keys": ":$-5,$d<CR>"},
    {"name": "exr_d_semicolon", "lines": ["abc", "def", "ghi"], "keys": ":2;+1d<CR>"},
    {"name": "exr_d_cursor_first_nonblank", "lines": ["abc", "  defgh", "ghi", "  jkl"], "keys": "$:2,3d<CR>"},
    {"name": "exr_d_cursor_through_end", "lines": ["abc", "  defgh", "ghi", "  jkl"], "keys": "$:2,4d<CR>"},
    {"name": "exr_m_cursor_last_line", "lines": ["abcd", "  defgh", "ghi", "  jkl"], "keys": "ll:2,3m$<CR>"},
    {"name": "exr_m_cursor_dollar", "lines": ["abc", "  defgh", "ghi", "  jkl"], "keys": "$:2,3m$<CR>"},
    {"name": "exr_t_cursor_last_line", "lines": ["abcd", "  defgh", "ghi", "  jkl"], "keys": "ll:2,3t0<CR>"},
    {"name": "exr_t_dest_past_end", "lines": ["abcd", "  defgh", "ghi", "  jkl"], "keys": "ll:t99<CR>"},
    {"name": "exr_m_dest_past_end", "lines": ["abcd", "  defgh", "ghi", "  jkl"], "keys": "ll:m99<CR>"},
    {"name": "exr_put_past_end", "lines": ["abcd", "  defgh", "ghi", "  jkl"], "keys": "yyll:99put<CR>"},
    {"name": "exr_goto_keeps_column", "lines": ["abcd", "  defgh", "ghi", "  jkl"], "keys": "ll:3<CR>"},
    {"name": "exr_goto_dollar", "lines": ["abcd", "  defgh", "ghi", "  jkl"], "keys": "ll:$<CR>"},
    {"name": "exr_goto_minus", "lines": ["abcd", "  defgh", "ghi", "  jkl"], "keys": "jjll:-<CR>"},
    {"name": "exr_goto_before_start", "lines": ["abcd", "  defgh", "ghi", "  jkl"], "keys": "jj:-5<CR>"},
    {"name": "exr_visual_offset", "lines": ["abcd", "  defgh", "ghi", "  jkl"], "keys": "Vj<Esc>G:'<,'>+1d<CR>"},
    {"name": "exr_s_to_dollar", "lines": ["abcd", "  defgh", "ghi", "  jkl"], "keys": ":3,$s/^/X/<CR>"},
    {"name": "exr_j_dot_dollar", "lines": ["abcd", "  defgh", "ghi", "  jkl"], "keys": ":.,$j<CR>"},
    #     EX ADDRESSES (#608, #1093, #724, #809, #801, #1087): patterns, marks, `;`, backwards ranges,
    #     a count after the command, and the Ex errors.
    {"name": "exa_pattern_forward", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":/b/d<CR>"},
    {"name": "exa_pattern_backward_wraps", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":?b?d<CR>"},
    {"name": "exa_pattern_forward_wraps", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": "G:/b/d<CR>"},
    {"name": "exa_pattern_own_line_last", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":/a1/d<CR>"},
    {"name": "exa_pattern_semicolon", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":/b/;/b/d<CR>"},
    {"name": "exa_pattern_comma_from_cursor", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":/b/,/b/d<CR>"},
    {"name": "exa_pattern_chained", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":/b//a/d<CR>"},
    {"name": "exa_pattern_offset", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":/b/+1d<CR>"},
    {"name": "exa_pattern_bare_minus", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":/b/-d<CR>"},
    {"name": "exa_pattern_to_dollar", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":/b/,$d<CR>"},
    {"name": "exa_pattern_from_line", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":3;/b/d<CR>"},
    {"name": "exa_pattern_space_before_command", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":/b/ d<CR>"},
    {"name": "exa_pattern_escaped_delimiter", "lines": ["a/1", "b2", "a/3", "b4", "a5"], "keys": ":/a\\/3/d<CR>"},
    {"name": "exa_pattern_bar_is_pattern", "lines": ["a|1", "b2", "a3", "b4", "a5"], "keys": "G:/a|1/d<CR>"},
    {"name": "exa_pattern_goto_keeps_column", "lines": ["a1x", "b2x", "a3x", "b4x", "a5x"], "keys": "ll:/b/<CR>"},
    {"name": "exa_pattern_unclosed_goto", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":/b<CR>"},
    {"name": "exa_pattern_sets_last_search", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":/b/d<CR>n"},
    {"name": "exa_pattern_N_after_address", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":/b/d<CR>N"},
    {"name": "exa_pattern_backward_sets_direction", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": "G:?b?d<CR>n"},
    {"name": "exa_pattern_empty_is_previous", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":/b/;//d<CR>"},
    {"name": "exa_pattern_then_substitute", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":/b/s/b/X/<CR>"},
    {"name": "exa_pattern_then_sort", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":/b/,/a5/sort!<CR>"},
    {"name": "exa_pattern_s_range", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":/a3/,$s/a/Z/<CR>"},
    {"name": "exa_last_search_forward", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": "/a3<CR>gg:\\/d<CR>"},
    {"name": "exa_last_search_backward", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": "G?a3<CR>G:\\?d<CR>"},
    {"name": "exa_last_search_wraps", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": "?a<CR>G:\\/d<CR>"},
    {"name": "exa_last_search_keeps_direction", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": "/a<CR>gg:\\/d<CR>N"},
    {"name": "exa_last_substitute", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":s/b/X/<CR>:\\&d<CR>"},
    {"name": "exa_err_pattern_not_found", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":/zz/d<CR>"},
    {"name": "exa_err_pattern_not_found_goto", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":/zz/<CR>"},
    {"name": "exa_err_pattern_after_semicolon", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":/b/;/zz/d<CR>"},
    {"name": "exa_err_no_previous_search", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":\\/d<CR>"},
    {"name": "exa_err_no_previous_search_backward", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":\\?d<CR>"},
    {"name": "exa_err_no_previous_substitute", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":\\&d<CR>"},
    {"name": "exa_mark_offsets", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": "jjmx:'x-1,'x+1d<CR>"},
    {"name": "exa_mark_goto", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": "Gmaggjmb:'a,'b<CR>"},
    {"name": "exa_mark_change_marks", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": "jyyjp:'[,']d<CR>"},
    {"name": "exa_mark_last_change", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": "jjxgg:'.d<CR>"},
    {"name": "exa_mark_context", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": "3GG:''d<CR>"},
    {"name": "exa_err_mark_context_unset", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":''d<CR>"},
    {"name": "exa_err_mark_unset_goto", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":'z<CR>"},
    {"name": "exa_err_mark_file", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":'Zd<CR>"},
    {"name": "exa_star_is_visual", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": "jVj<Esc>gg:*d<CR>"},
    {"name": "exa_visual_colon", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": "jVj:d<CR>"},
    {"name": "exa_leading_comma", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": "jj:,d<CR>"},
    {"name": "exa_leading_semicolon", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": "jj:;+1d<CR>"},
    {"name": "exa_comma_offset", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": "jj:,+1d<CR>"},
    {"name": "exa_chained_offsets", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":2+3-2d<CR>"},
    {"name": "exa_semicolon_cursor_kept_on_error", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":2;99d<CR>"},
    {"name": "exa_semicolon_cursor_after_yank", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": "l:3;+1y<CR>"},
    {"name": "exa_backwards_yes", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":3,2d<CR>y"},
    {"name": "exa_backwards_no", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":3,2d<CR>n"},
    {"name": "exa_backwards_esc", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":4,2d<CR><Esc>x"},
    {"name": "exa_backwards_other_key_ignored", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":4,2d<CR>xy"},
    {"name": "exa_backwards_then_keys", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":4,2d<CR>yx"},
    {"name": "exa_backwards_marks", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": "jmajjmb:'b,'ad<CR>y"},
    {"name": "exa_backwards_rest_of_line", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":4,2d|1d<CR>y"},
    {"name": "exa_backwards_no_drops_line", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":4,2d|1d<CR>n"},
    {"name": "exa_backwards_goto_no_prompt", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":3,2<CR>"},
    {"name": "exa_backwards_past_end_e16", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":9,2d<CR>y"},
    {"name": "exa_backwards_semicolon", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":$;-2d<CR>n"},
    {"name": "exa_backwards_pattern", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":5,/b/d<CR>n"},
    {"name": "exa_backwards_join", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":4,2j<CR>y"},
    {"name": "exa_backwards_substitute", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":3,2s/a/X/<CR>y"},
    {"name": "exa_backwards_in_macro", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": "qa:4,2d<CR>yq"},
    {"name": "exa_count_d", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":d 3<CR>"},
    {"name": "exa_count_after_line", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":2d 2<CR>"},
    {"name": "exa_count_after_range", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":2,3d 2<CR>"},
    {"name": "exa_count_with_register", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":d a 2<CR>"},
    {"name": "exa_count_clamps", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":4d 9<CR>"},
    {"name": "exa_count_after_percent", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":%d 2<CR>"},
    {"name": "exa_count_y", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":y 2<CR>"},
    {"name": "exa_count_j", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":j 3<CR>"},
    {"name": "exa_count_j_bang", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":j! 2<CR>"},
    {"name": "exa_count_shift", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":> 2<CR>", "setup": "set shiftwidth=4 expandtab startofline"},
    {"name": "exa_register_d", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":d a<CR>"},
    {"name": "exa_register_blackhole", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":d _<CR>"},
    {"name": "exa_register_append", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":y a|2y A<CR>G\"ap"},
    {"name": "exa_err_count_zero", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":d 0<CR>"},
    {"name": "exa_err_trailing_y", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":y a b<CR>"},
    {"name": "exa_err_trailing_j", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":j x<CR>"},
    {"name": "exa_err_trailing_shift", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":> x<CR>"},
    {"name": "exa_abbrev_de", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":de<CR>"},
    {"name": "exa_abbrev_yan", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":2yan<CR>P"},
    {"name": "exa_m_onto_itself", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":2m2<CR>"},
    {"name": "exa_m_onto_itself_keeps_column", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": "jl:2m2<CR>"},
    {"name": "exa_m_before_itself", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":2m1<CR>"},
    {"name": "exa_m_range_before_itself", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":2,3m1<CR>"},
    {"name": "exa_m_range_after_itself", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":2,3m3<CR>"},
    {"name": "exa_err_m_into_itself", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":1,3m2<CR>"},
    {"name": "exa_err_m_no_destination", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":m<CR>"},
    {"name": "exa_err_t_no_destination", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":t<CR>"},
    {"name": "exa_r_bang_no_space", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":r!echo hi<CR>"},
    {"name": "exa_r_bang_no_space_line_zero", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":0r!echo hi<CR>"},
    {"name": "exa_err_filter_past_end", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":99!tr a-z A-Z<CR>"},
    {"name": "exa_err_y_past_end", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":99y<CR>"},
    {"name": "exa_err_t_from_past_end", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":99t0<CR>"},
    {"name": "exa_err_m_from_past_end", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":99m0<CR>"},
    {"name": "exa_err_d_range_past_end", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":99,100d<CR>"},
    {"name": "exa_err_sign", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":sign<CR>"},
    {"name": "exa_err_norm_bang", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":norm!<CR>"},
    {"name": "exa_err_set_number_required", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":set sw=x<CR>"},
    {"name": "exa_err_set_no_on_number", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":set nosw<CR>"},
    {"name": "exa_err_set_unknown_with_value", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":set foo=1<CR>"},
    {"name": "exa_set_unsupported_is_quiet", "lines": ["a1", "b2", "a3", "b4", "a5"], "keys": ":set number<CR>"},
    #     SCENARIO CLASSES (#901): the classes `tools/parity/scenario_classes.py` found thin. Errors assert
    #     the E-number (`emsg`) and the state after the failure; the keys after a failed command still run,
    #     as typed keys do. Registers, marks, macros, multibyte text and empty or tiny buffers.
    #     error: E353 and the special registers' own errors on a put from an empty register ------------
    {"name": "sc_err_p_empty_unnamed", "lines": ["abc"], "keys": "p"},
    {"name": "sc_err_p_empty_named", "lines": ["abc"], "keys": '"ap'},
    {"name": "sc_err_P_empty_named", "lines": ["abc"], "keys": '"aP'},
    {"name": "sc_err_p_empty_reg0", "lines": ["abc"], "keys": '"0p'},
    {"name": "sc_err_p_empty_numbered", "lines": ["abc"], "keys": '"1p'},
    {"name": "sc_err_p_empty_small_delete", "lines": ["abc"], "keys": '"-p'},
    {"name": "sc_err_p_empty_uppercase", "lines": ["abc"], "keys": '"Ap'},
    {"name": "sc_err_gp_empty_named", "lines": ["abc"], "keys": '"agp'},
    {"name": "sc_err_bracket_p_empty_named", "lines": ["abc"], "keys": '"a]p'},
    {"name": "sc_err_count_p_empty_named", "lines": ["abc"], "keys": '"a3p'},
    {"name": "sc_err_p_empty_then_keys_run", "lines": ["abc"], "keys": '"ap$x'},
    {"name": "sc_err_p_empty_register_not_kept", "lines": ["abc"], "keys": '"apyl$p'},
    {"name": "sc_reg_p_blackhole_is_quiet", "lines": ["abc"], "keys": '"_p'},
    {"name": "sc_err_p_colon_no_command", "lines": ["abc"], "keys": '":p'},
    {"name": "sc_err_p_slash_no_pattern", "lines": ["abc"], "keys": '"/p'},
    {"name": "sc_err_p_dot_no_insert", "lines": ["abc"], "keys": '".p'},
    {"name": "sc_err_p_percent_no_file", "lines": ["abc"], "keys": '"%p'},
    {"name": "sc_err_at_colon_no_command", "lines": ["abc"], "keys": "@:"},
    {"name": "sc_err_dp_not_diff", "lines": ["abc"], "keys": "dp"},
    {"name": "sc_err_do_not_diff", "lines": ["abc", "def"], "keys": "jdo"},
    #     error: marks -------------------------------------------------------------------------------------
    {"name": "sc_err_quote_mark_unset", "lines": ["abc", "def"], "keys": "j'a"},
    {"name": "sc_err_backtick_mark_unset", "lines": ["abc", "def"], "keys": "jl`a"},
    {"name": "sc_err_d_quote_mark_unset", "lines": ["abc", "def"], "keys": "d'a"},
    {"name": "sc_err_y_backtick_mark_unset", "lines": ["abc", "def"], "keys": "y`aP"},
    {"name": "sc_err_mark_unset_then_keys_run", "lines": ["abc", "def"], "keys": "j'ax"},
    {"name": "sc_err_mark_on_deleted_line", "lines": ["a", "b", "c"], "keys": "jmaddgg'a"},
    {"name": "sc_err_ex_range_mark_unset", "lines": ["a", "b", "c"], "keys": ":'a,'bd<CR>"},
    {"name": "sc_err_bracket_mark_unset", "lines": ["abc", "def"], "keys": "j`["},
    #     error: search and substitute -------------------------------------------------------------------
    {"name": "sc_err_search_not_found", "lines": ["abc", "def"], "keys": "/zzz<CR>"},
    {"name": "sc_err_search_back_not_found", "lines": ["abc", "def"], "keys": "j?zzz<CR>"},
    {"name": "sc_err_search_not_found_then_keys_run", "lines": ["abc", "def"], "keys": "l/zzz<CR>x"},
    {"name": "sc_err_d_search_not_found", "lines": ["abc def"], "keys": "d/zzz<CR>"},
    {"name": "sc_err_c_search_not_found", "lines": ["abc def"], "keys": "c/zzz<CR>"},
    {"name": "sc_err_y_search_not_found", "lines": ["abc def"], "keys": "y/zzz<CR>P"},
    {"name": "sc_err_n_no_previous_pattern", "lines": ["abc"], "keys": "n"},
    {"name": "sc_err_N_no_previous_pattern", "lines": ["abc"], "keys": "N"},
    {"name": "sc_err_dn_no_previous_pattern", "lines": ["abc"], "keys": "dn"},
    {"name": "sc_err_n_after_failed_search", "lines": ["abc"], "keys": "/zzz<CR>n"},
    {"name": "sc_err_star_on_blank_line", "lines": ["", "abc"], "keys": "*"},
    {"name": "sc_star_on_punctuation_searches_it", "lines": ["...", "abc"], "keys": "*"},
    {"name": "sc_star_on_punctuation_escapes_magic", "lines": ["x  .*.", "a.b", ".*. y"], "keys": "$*"},
    {"name": "sc_star_on_punctuation_then_n", "lines": ["  ;;", ";; b ;;", ";;"], "keys": "*nx"},
    {"name": "sc_hash_on_punctuation_searches_back", "lines": ["/?", "a /? b"], "keys": "j0#x"},
    {"name": "sc_star_prefers_keyword_after_punctuation", "lines": [".:. foo", ".:.", "foo"], "keys": "*x"},
    {"name": "sc_gstar_on_punctuation", "lines": ["~[", "x~[y"], "keys": "g*x"},
    {"name": "sc_err_sub_not_found", "lines": ["abc"], "keys": ":s/zzz/y/<CR>"},
    {"name": "sc_err_sub_all_not_found", "lines": ["abc", "def"], "keys": ":%s/zzz/y/<CR>"},
    {"name": "sc_sub_e_flag_is_quiet", "lines": ["abc"], "keys": ":s/zzz/y/e<CR>"},
    {"name": "sc_err_sub_empty_pattern_no_previous", "lines": ["abc"], "keys": ":s//y/<CR>"},
    {"name": "sc_err_amp_no_previous_sub", "lines": ["abc"], "keys": "&"},
    {"name": "sc_global_not_found_is_quiet", "lines": ["abc", "def"], "keys": ":g/zzz/d<CR>"},
    {"name": "sc_global_delete_fills_unnamed", "lines": ["a1", "b", "a2", "c"], "keys": ":g/a/d<CR>"},
    {"name": "sc_global_delete_range_then_p", "lines": ["a1", "a2", "b", "a3", "a4"], "keys": ":2,4g/a/d<CR>p"},
    {"name": "sc_global_delete_numbered_ring", "lines": ["a1", "b", "a2", "c", "a3"], "keys": ":g/a/d<CR>\"3p"},
    {"name": "sc_err_sub_not_found_cursor_stays", "lines": ["abc", "def"], "keys": "jl:s/zzz/y/<CR>x"},
    #     error: Ex --------------------------------------------------------------------------------------
    {"name": "sc_err_ex_unknown_command", "lines": ["abc"], "keys": ":foo<CR>"},
    {"name": "sc_err_ex_unknown_then_keys_run", "lines": ["abc"], "keys": ":foo<CR>x"},
    {"name": "sc_err_ex_write_no_file_name", "lines": ["abc"], "keys": ":w<CR>"},
    {"name": "sc_err_ex_close_last_window", "lines": ["abc"], "keys": ":close<CR>"},
    {"name": "sc_err_ctrl_w_c_last_window", "lines": ["abc"], "keys": "<C-w>c"},
    {"name": "sc_err_ex_normal_no_argument", "lines": ["abc"], "keys": ":normal<CR>"},
    {"name": "sc_err_ex_set_unknown_option", "lines": ["abc"], "keys": ":set nosuchoption<CR>"},
    {"name": "sc_err_ex_trailing_characters", "lines": ["a", "b"], "keys": ":d x y<CR>"},
    {"name": "sc_err_ex_bad_pattern", "lines": ["abc"], "keys": ":s/\\(/x/<CR>"},
    {"name": "sc_err_search_bad_pattern", "lines": ["abc"], "keys": "/a\\{<CR>"},
    {"name": "sc_err_search_unmatched_open", "lines": ["abc"], "keys": "/\\(<CR>"},
    {"name": "sc_err_search_unmatched_close", "lines": ["abc"], "keys": "/\\)<CR>x"},
    {"name": "sc_err_ex_unmatched_close", "lines": ["abc"], "keys": ":s/\\)/x/<CR>"},
    {"name": "sc_err_ex_bad_brace", "lines": ["abc"], "keys": ":s/a\\{/x/<CR>"},
    {"name": "sc_err_ex_global_bad_pattern", "lines": ["abc"], "keys": ":g/\\(/d<CR>"},
    {"name": "sc_err_ex_sort_bad_pattern", "lines": ["b", "a"], "keys": ":sort /\\(/<CR>"},
    {"name": "sc_err_sub_count_bad_pattern", "lines": ["abc"], "keys": ":s/\\(/x/n<CR>"},
    {"name": "sc_sub_empty_pattern_after_search", "lines": ["abc"], "keys": "/b<CR>:s//y/<CR>"},
    #     failed motions (beep, no E-number) and the state after them -----------------------------------
    {"name": "sc_fail_f_not_found_then_x", "lines": ["abc"], "keys": "lfzx"},
    {"name": "sc_fail_d_f_not_found", "lines": ["abc"], "keys": "dfz"},
    {"name": "sc_fail_d_f_not_found_then_x", "lines": ["abc"], "keys": "dfzx"},
    {"name": "sc_fail_count_f_too_few", "lines": ["axbxc"], "keys": "3fxx"},
    {"name": "sc_fail_d_count_t_too_few", "lines": ["axbxc"], "keys": "d3tx"},
    {"name": "sc_fail_c_f_not_found_then_X", "lines": ["abc"], "keys": "llcfzX<Esc>"},
    {"name": "sc_fail_y_f_not_found_keeps_register", "lines": ["abc def"], "keys": "yiwwyfzP"},
    {"name": "sc_fail_F_at_col0", "lines": ["abc"], "keys": "Fax"},
    {"name": "sc_fail_semicolon_no_previous_find", "lines": ["abc"], "keys": ";x"},
    {"name": "sc_fail_comma_no_previous_find", "lines": ["abc"], "keys": "l,x"},
    {"name": "sc_fail_k_on_first_line_then_x", "lines": ["abc", "def"], "keys": "lkx"},
    {"name": "sc_fail_dk_on_first_line_then_x", "lines": ["abc", "def"], "keys": "dkx"},
    {"name": "sc_fail_j_on_last_line_then_x", "lines": ["abc", "def"], "keys": "jljx"},
    {"name": "sc_count_j_past_end_clamps", "lines": ["a", "b", "c"], "keys": "5jx"},
    {"name": "sc_count_d_j_past_end_clamps", "lines": ["a", "b", "c"], "keys": "jd5j"},
    {"name": "sc_fail_percent_no_bracket", "lines": ["abc"], "keys": "%x"},
    {"name": "sc_fail_d_percent_no_bracket", "lines": ["abc"], "keys": "d%"},
    {"name": "sc_fail_percent_unmatched", "lines": ["(abc"], "keys": "%x"},
    {"name": "sc_fail_w_at_buffer_end", "lines": ["abc"], "keys": "$wx"},
    {"name": "sc_fail_b_at_buffer_start", "lines": ["abc"], "keys": "bx"},
    {"name": "sc_ci_paren_seeks_forward", "lines": ["ab (c) d"], "keys": "ci)X<Esc>"},
    {"name": "sc_fail_di_quote_no_quote", "lines": ["abc"], "keys": 'di"x'},
    {"name": "sc_fail_count_r_past_end", "lines": ["abc"], "keys": "l5rx"},
    {"name": "sc_fail_count_r_past_end_then_x", "lines": ["abc"], "keys": "l5rxx"},
    {"name": "sc_count_tilde_past_end_clamps", "lines": ["abc"], "keys": "l9~"},
    {"name": "sc_quiet_x_on_empty_line", "lines": ["", "abc"], "keys": "x"},
    {"name": "sc_quiet_X_at_col0", "lines": ["abc"], "keys": "X"},
    {"name": "sc_fail_h_at_col0_then_x", "lines": ["abc"], "keys": "hx"},
    {"name": "sc_fail_ctrl_a_no_number", "lines": ["abc"], "keys": "<C-a>"},
    {"name": "sc_fail_gv_no_previous_visual", "lines": ["abc"], "keys": "gvd"},
    {"name": "sc_quiet_d_brace_back_at_top", "lines": ["abc", "def"], "keys": "d{"},
    {"name": "sc_quiet_section_back_at_top", "lines": ["abc", "def"], "keys": "[[x"},
    {"name": "sc_fail_redo_at_newest", "lines": ["abc"], "keys": "x<C-r>", "setup": UNDO_SETUP},
    {"name": "sc_fail_at_empty_register", "lines": ["abc"], "keys": "@ax"},
    {"name": "sc_fail_at_at_no_previous", "lines": ["abc"], "keys": "@@x"},
    #     registers -------------------------------------------------------------------------------------
    {"name": "sc_reg_two_named_registers", "lines": ["one", "two"], "keys": '"ayyj"byy"ap"bp'},
    {"name": "sc_reg_append_charwise_to_linewise", "lines": ["one", "two"], "keys": '"ayyj"Ayiw"ap'},
    {"name": "sc_reg_append_linewise_to_charwise", "lines": ["one", "two"], "keys": '"ayiwj"Ayy"ap'},
    {"name": "sc_reg_append_to_empty", "lines": ["one", "two"], "keys": '"Ayyj"ap'},
    {"name": "sc_reg_blackhole_keeps_unnamed", "lines": ["one", "two", "three"], "keys": 'yyj"_ddp'},
    {"name": "sc_reg_blackhole_x_keeps_unnamed", "lines": ["abc def"], "keys": 'yiw$"_xP'},
    {"name": "sc_reg_numbered_shift", "lines": ["1", "2", "3", "4"], "keys": 'dddddd"2p'},
    {"name": "sc_reg_numbered_shift_third", "lines": ["1", "2", "3", "4"], "keys": 'dddddd"3p'},
    {"name": "sc_reg_yank_skips_numbered", "lines": ["1", "2"], "keys": 'ddyy"1p'},
    {"name": "sc_reg_small_delete_to_minus", "lines": ["abc def"], "keys": 'dw$"-p'},
    {"name": "sc_reg_small_delete_skips_numbered", "lines": ["one two", "three"], "keys": 'jddkdw"1p'},
    {"name": "sc_reg_multiline_charwise_to_numbered", "lines": ["ab", "cd", "ef"], "keys": 'ld}"1p'},
    {"name": "sc_reg_paragraph_delete_to_numbered", "lines": ["one two", "three"], "keys": 'wd}"1P'},
    {"name": "sc_reg_named_delete_shifts_numbered", "lines": ["1", "2", "3"], "keys": 'dd"add"1p'},
    {"name": "sc_reg0_after_yank_then_delete", "lines": ["one", "two", "three"], "keys": 'yyjdd"0p'},
    {"name": "sc_reg_numbered_dot_increments", "lines": ["1", "2", "3", "x"], "keys": 'dddddd"1pu.'},
    {"name": "sc_reg_numbered_dot_increments_twice", "lines": ["1", "2", "3", "x"], "keys": 'dddddd"1pu.u.'},
    {"name": "sc_reg_named_put_count_linewise", "lines": ["one", "two"], "keys": '"ayy"a3p'},
    {"name": "sc_reg_named_put_count_charwise", "lines": ["ab"], "keys": '"ayl"a3p'},
    {"name": "sc_reg_colon_after_ex", "lines": ["abc"], "keys": ':s/b/x/<CR>":p'},
    {"name": "sc_reg_slash_after_search", "lines": ["abc bcd"], "keys": '/bc<CR>"/P'},
    {"name": "sc_reg_dot_after_insert", "lines": ["abc"], "keys": 'iXY<Esc>$".p'},
    {"name": "sc_reg_insert_ctrl_r_named", "lines": ["abc"], "keys": '"ayiwA <C-r>a<Esc>'},
    {"name": "sc_reg_insert_ctrl_r_unnamed", "lines": ["abc def"], "keys": 'yiwA-<C-r>"<Esc>'},
    {"name": "sc_reg_insert_ctrl_r_linewise", "lines": ["abc", "def"], "keys": 'yyjA<C-r>"<Esc>'},
    {"name": "sc_reg_insert_ctrl_r_empty", "lines": ["abc"], "keys": "A<C-r>b<Esc>"},
    {"name": "sc_reg_cmdline_ctrl_r_named", "lines": ["abc", "abc"], "keys": '"ayl:%s/<C-r>a/X/<CR>'},
    {"name": "sc_reg_execute_yanked_text", "lines": ["dw", "foo bar"], "keys": '"ay$j@a'},
    {"name": "sc_reg_visual_yank_named", "lines": ["abc def"], "keys": 'v"ay$"ap'},
    {"name": "sc_reg_visual_delete_named", "lines": ["abc def"], "keys": 'wve"ad0"aP'},
    {"name": "sc_reg_named_then_unnamed_points", "lines": ["one", "two"], "keys": '"ayyjp'},
    {"name": "sc_reg_uppercase_append_unnamed_is_whole", "lines": ["a b"], "keys": '"ayiww"Ayiwo<C-r>"<Esc>'},
    {"name": "sc_reg_x_then_named_paste_order", "lines": ["abc"], "keys": '"ax"bx"ap"bp'},
    {"name": "sc_reg_yank_multibyte_word", "lines": ["가나 다라"], "keys": 'w"ayiw0"aP'},
    #     marks -----------------------------------------------------------------------------------------
    {"name": "sc_mark_jump_line_first_non_blank", "lines": ["  abc", "def"], "keys": "$majj'a"},
    {"name": "sc_mark_jump_exact_column", "lines": ["abc", "def"], "keys": "llmaj0`a"},
    {"name": "sc_mark_d_quote_linewise", "lines": ["a", "b", "c", "d"], "keys": "majjd'a"},
    {"name": "sc_mark_backtick_jump_back", "lines": ["abc def ghi"], "keys": "wmaw`ax"},
    {"name": "sc_mark_d_backtick_back", "lines": ["abc def ghi"], "keys": "wmawd`a"},
    {"name": "sc_mark_y_quote_back", "lines": ["a", "b", "c"], "keys": "majjy'aP"},
    {"name": "sc_mark_moves_with_line_above", "lines": ["a", "b"], "keys": "jmakOx<Esc>gg'ax"},
    {"name": "sc_mark_moves_with_deleted_line_above", "lines": ["a", "b", "c"], "keys": "jjmaggdd'ax"},
    {"name": "sc_mark_ex_range", "lines": ["a", "b", "c", "d"], "keys": "jmajmb:'a,'bd<CR>"},
    {"name": "sc_mark_ex_mark_command", "lines": ["a", "b", "c"], "keys": ":2mark a<CR>gg'ax"},
    {"name": "sc_mark_ex_k_command", "lines": ["a", "b", "c"], "keys": ":3ka<CR>'ax"},
    {"name": "sc_mark_overwrite", "lines": ["a", "b", "c"], "keys": "majmagg'ax"},
    {"name": "sc_mark_uppercase", "lines": ["a", "b", "c"], "keys": "jmAgg'Ax"},
    {"name": "sc_mark_last_change_line", "lines": ["abc", "def", "ghi"], "keys": "jlxgg'.x"},
    {"name": "sc_mark_last_change_exact", "lines": ["abc", "def", "ghi"], "keys": "jllxgg`.x"},
    {"name": "sc_mark_last_insert", "lines": ["abc", "def"], "keys": "jAxy<Esc>gg`^x"},
    {"name": "sc_mark_change_start_end_after_yank", "lines": ["abc def ghi"], "keys": "wyiw0`]x"},
    {"name": "sc_mark_change_start_after_put", "lines": ["abc", "def"], "keys": "yyjp`[x"},
    {"name": "sc_mark_change_end_after_put", "lines": ["abc def"], "keys": "yiw$p`]x"},
    {"name": "sc_mark_jump_back_after_G", "lines": ["a", "b", "c"], "keys": "jG``x"},
    {"name": "sc_mark_jump_back_line_after_search", "lines": ["  a", "b", "c x"], "keys": "/x<CR>''x"},
    {"name": "sc_mark_jump_back_twice", "lines": ["a", "b", "c"], "keys": "G````x"},
    {"name": "sc_mark_visual_start_end", "lines": ["abcdef"], "keys": "lvll<Esc>0`<x`>x"},
    {"name": "sc_mark_visual_line_marks", "lines": ["a", "b", "c", "d"], "keys": "jVj<Esc>gg:'<,'>d<CR>"},
    {"name": "sc_mark_d_to_visual_end", "lines": ["abc def ghi"], "keys": "wviw<Esc>0d`>"},
    {"name": "sc_mark_multibyte_column", "lines": ["가나다라"], "keys": "llma0`ax"},
    {"name": "sc_mark_survives_undo", "lines": ["a", "b", "c"], "keys": "jjmaggddu'ax", "setup": UNDO_SETUP},
    # #686: marks follow edits as nvim's mark_adjust moves them.
    {"name": "sc_mark_keeps_col_after_x_before", "lines": ["abcdefgh"], "keys": "5|ma0x`ax"},
    {"name": "sc_mark_follows_join", "lines": ["aa", "  bbbb"], "keys": "j3|makJ`ax"},
    {"name": "sc_mark_deleted_line_restored_by_undo", "lines": ["a", "b", "c"], "keys": "jmaddu`ax",
     "setup": UNDO_SETUP},
    {"name": "sc_mark_redo_drops_it_again", "lines": ["a", "b", "c"], "keys": "jmaddu<C-r>gg'ax",
     "setup": UNDO_SETUP},
    {"name": "sc_visual_marks_follow_dd_above", "lines": ["a", "bcd", "e"], "keys": "jlvl<Esc>ggddgvd"},
    {"name": "sc_err_context_mark_empty_jumplist", "lines": ["a", "b"], "keys": "j''x"},
    {"name": "sc_jumplist_follows_dd_above", "lines": ["a", "b", "c", "d"], "keys": "Gggjdd<C-o>x"},
    {"name": "sc_mark_c_quote", "lines": ["a", "b", "c"], "keys": "jmajc'aX<Esc>"},
    #     macros ----------------------------------------------------------------------------------------
    {"name": "sc_mac_record_replay", "lines": ["abcdef"], "keys": "qaxq@a"},
    {"name": "sc_mac_count_replay", "lines": ["abcdef"], "keys": "qaxq3@a"},
    {"name": "sc_mac_at_at_repeats", "lines": ["abcdef"], "keys": "qaxq@a@@"},
    {"name": "sc_mac_register_holds_keys", "lines": ["abc def"], "keys": 'qadwq"ap'},
    {"name": "sc_mac_insert_in_macro", "lines": ["a", "b"], "keys": "qaIX<Esc>jq@a"},
    {"name": "sc_mac_append_uppercase", "lines": ["abcdef"], "keys": "qaxqqAlq0@a"},
    {"name": "sc_mac_search_in_macro", "lines": ["a x b x c x"], "keys": "qa/x<CR>rYq@a"},
    {"name": "sc_mac_ex_in_macro", "lines": ["a", "a", "a"], "keys": "qa:s/a/b/<CR>jq@a"},
    {"name": "sc_mac_dot_after_replay", "lines": ["abcdef"], "keys": "qaxq@a."},
    {"name": "sc_mac_undo_after_replay", "lines": ["abcdef"], "keys": "qaxxq@au", "setup": UNDO_SETUP},
    {"name": "sc_mac_line_per_replay", "lines": ["a1", "a2", "a3"], "keys": "qa0xjq@a@a"},
    {"name": "sc_mac_failed_motion_aborts_replay", "lines": ["a1", "a2", "a3"], "keys": "qa0xjq5@a"},
    {"name": "sc_mac_failed_find_aborts_rest", "lines": ["abc", "abc"], "keys": "qafzxq0j@a"},
    #     (A self-calling macro that ends on a failed `j` is left out: ruse has no failure to stop it, so it
    #     runs to the 100 000-key recursion cap, which is minutes in a debug test build. #1085.)
    {"name": "sc_mac_at_colon_repeats_ex", "lines": ["a", "a"], "keys": ":s/a/b/<CR>j@:"},
    {"name": "sc_mac_normal_runs_register", "lines": ["abc", "abc"], "keys": "qaxq:%normal @a<CR>"},
    {"name": "sc_mac_empty_recording_clears", "lines": ["abc"], "keys": '"ayyqaq"ap'},
    {"name": "sc_mac_record_with_count_inside", "lines": ["abcdefgh"], "keys": "qa2xq@a"},
    {"name": "sc_mac_multibyte", "lines": ["가나다라마"], "keys": "qaxq2@a"},
    {"name": "sc_mac_count_at_at", "lines": ["abcdefgh"], "keys": "qaxq@a3@@"},
    {"name": "sc_mac_yank_put_in_macro", "lines": ["one", "two"], "keys": "qayypjq@a"},
    {"name": "sc_mac_visual_in_macro", "lines": ["abc def ghi"], "keys": "qavedq@a"},
    {"name": "sc_mac_text_object_in_macro", "lines": ["aa bb cc dd"], "keys": "qaciwX<Esc>wq@a"},
    {"name": "sc_mac_dot_inside_macro", "lines": ["abcdefgh"], "keys": "xqa.q@a"},
    {"name": "sc_mac_mark_in_macro", "lines": ["a", "b", "c"], "keys": "qamajq@a'ax"},
    {"name": "sc_mac_ctrl_a_counted_replay", "lines": ["x 1"], "keys": "qa<C-a>q3@a"},
    {"name": "sc_mac_insert_with_cr", "lines": ["ab"], "keys": "qaA!<CR>x<Esc>q@a"},
    {"name": "sc_mac_find_repeat_in_macro", "lines": ["a.b.c.d.e"], "keys": "qaf.;xq0@a"},
    {"name": "sc_mac_search_n_in_macro", "lines": ["x a x b x c"], "keys": "/x<CR>qanrYq@a"},
    {"name": "sc_mac_execute_unnamed", "lines": ["x", "abc"], "keys": 'yljj@"'},
    {"name": "sc_mac_execute_reg0", "lines": ["x", "abc"], "keys": "ylj@0"},
    {"name": "sc_mac_uppercase_replay_same", "lines": ["abcdef"], "keys": "qaxq@A"},
    {"name": "sc_mac_recorded_register_is_charwise", "lines": ["abc"], "keys": 'qaxq"aP'},
    {"name": "sc_mac_overwrite_recording", "lines": ["abcdef"], "keys": "qaxqqalq0@a"},
    {"name": "sc_mac_ex_bar_in_macro", "lines": ["a", "a", "a"], "keys": "qa:s/a/b/|+1<CR>q@a"},
    {"name": "sc_mac_count_inside_and_outside", "lines": ["abcdefghijkl"], "keys": "qa2xq2@a"},
    {"name": "sc_mac_linewise_visual_runs_per_line", "lines": ["ab", "cd", "ef"], "keys": "qaxqjVj@a"},
    {"name": "sc_mac_replay_into_insert", "lines": ["ab"], "keys": "qaAz<Esc>q@a@a"},
    {"name": "sc_mac_numbered_register_record", "lines": ["abcdef"], "keys": "q1xq@1"},
    {"name": "sc_mac_motion_only", "lines": ["ab cd ef gh"], "keys": "qawq@ax"},
    {"name": "sc_mac_counted_insert_replay", "lines": ["ab"], "keys": "qaiX<Esc>q3@a"},
    {"name": "sc_mac_join_in_macro", "lines": ["a", "b", "c", "d"], "keys": "qaJq@a"},
    #     Visual `:` / `!` then `<Esc>`: the colon operator leaves the cursor at the area's start (typed `<Esc>`
    #     abandons the line; an untyped one would run it).
    {"name": "sc_v_colon_esc_charwise_forward", "lines": ["abcd", "efgh"], "keys": "lvjl:<Esc>x"},
    {"name": "sc_v_colon_esc_charwise_backward", "lines": ["abcd", "efgh"], "keys": "jllvkh:<Esc>x"},
    {"name": "sc_v_colon_esc_linewise_forward", "lines": ["abcd", "efgh"], "keys": "llVj:<Esc>x"},
    {"name": "sc_v_colon_esc_linewise_backward", "lines": ["abcd", "efgh"], "keys": "jllVk:<Esc>x"},
    {"name": "sc_v_colon_esc_linewise_one_line", "lines": ["abcd", "efgh"], "keys": "llV:<Esc>x"},
    {"name": "sc_v_colon_esc_linewise_mid", "lines": ["abcd", "efgh", "ijkl"], "keys": "jllVj:<Esc>x"},
    {"name": "sc_v_colon_esc_block_left_corner", "lines": ["abcd", "efgh"], "keys": "ll<C-v>jh:<Esc>x"},
    {"name": "sc_v_colon_esc_block_up_right", "lines": ["abcd", "efgh"], "keys": "jl<C-v>kl:<Esc>x"},
    {"name": "sc_v_bang_esc_charwise", "lines": ["abcd", "efgh"], "keys": "lvjl!<Esc>x"},
    {"name": "sc_v_colon_esc_then_gv", "lines": ["abcd", "efgh"], "keys": "jllvkh:<Esc>gvd"},
    #     multibyte -------------------------------------------------------------------------------------
    {"name": "sc_mb_x_hangul", "lines": ["가나다"], "keys": "lx"},
    {"name": "sc_mb_count_x_hangul", "lines": ["가나다라"], "keys": "l2x"},
    {"name": "sc_mb_X_hangul", "lines": ["가나다"], "keys": "$X"},
    {"name": "sc_mb_dollar_lands_on_char_start", "lines": ["ab가"], "keys": "$x"},
    {"name": "sc_mb_f_hangul", "lines": ["a가b나c"], "keys": "f나x"},
    {"name": "sc_mb_t_hangul", "lines": ["a가b나c"], "keys": "t나x"},
    {"name": "sc_mb_f_not_found", "lines": ["a가b"], "keys": "f나x"},
    {"name": "sc_mb_r_with_hangul", "lines": ["abc"], "keys": "lr가"},
    {"name": "sc_mb_r_hangul_with_ascii", "lines": ["가나다"], "keys": "lrx"},
    {"name": "sc_mb_tilde_latin", "lines": ["äöü"], "keys": "3~"},
    {"name": "sc_mb_gU_latin_word", "lines": ["straße x"], "keys": "gUiw"},
    # --- Case conversion conformance (#912, #732): nvim recases one character to one by its own
    #     toupper/tolower (utf8proc), keeps composing characters, and has no special casing. -----------
    {"name": "cc_tilde_sharp_s", "lines": ["ßa"], "keys": "~"},
    {"name": "cc_tilde_capital_sharp_s", "lines": ["ẞa"], "keys": "~"},
    {"name": "cc_tilde_count_mixed", "lines": ["ßǅﬁıx"], "keys": "5~"},
    {"name": "cc_gUU_sharp_s", "lines": ["straße"], "keys": "gUU"},
    {"name": "cc_guu_capital_sharp_s", "lines": ["STRAẞE"], "keys": "guu"},
    {"name": "cc_gUU_ligatures_kept", "lines": ["ﬁsh ﬀ ﬆ"], "keys": "gUU"},
    {"name": "cc_gUU_titlecase", "lines": ["ǅemal ǈ ǋ ǲ"], "keys": "gUU"},
    {"name": "cc_guu_titlecase_kept", "lines": ["ǅEMAL ǈ ǋ ǲ"], "keys": "guu"},
    {"name": "cc_g_tilde_titlecase", "lines": ["ǅx ǆ Ǆ"], "keys": "g~~"},
    {"name": "cc_gUU_turkish", "lines": ["ı i İ"], "keys": "gUU"},
    {"name": "cc_guu_turkish", "lines": ["İ I ı"], "keys": "guu"},
    {"name": "cc_g_tilde_turkish", "lines": ["İı"], "keys": "g~~"},
    {"name": "cc_gUU_long_s_micro", "lines": ["ſµÿ"], "keys": "gUU"},
    {"name": "cc_guu_greek_sigma", "lines": ["ΟΔΟΣ ΣΑ"], "keys": "guu"},
    {"name": "cc_gUU_greek_final_sigma", "lines": ["οδος"], "keys": "gUU"},
    {"name": "cc_gUU_iota_subscript", "lines": ["ᾳ ᾀ ΐ ῳ"], "keys": "gUU"},
    {"name": "cc_guu_iota_subscript", "lines": ["ᾼ ᾈ ῼ"], "keys": "guu"},
    {"name": "cc_gUU_composing_kept", "lines": ["éx i̇"], "keys": "gUU"},
    {"name": "cc_tilde_composing", "lines": ["éx"], "keys": "2~"},
    {"name": "cc_gUU_armenian_ligature", "lines": ["և ﬓ"], "keys": "gUU"},
    {"name": "cc_gUU_newer_letters", "lines": ["ꟍ ꟑ ꞔ"], "keys": "gUU"},
    {"name": "cc_gUU_cherokee", "lines": ["ꭰꭱ"], "keys": "gUU"},
    {"name": "cc_guu_cherokee", "lines": ["ᎠᎡ"], "keys": "guu"},
    {"name": "cc_gUiw_cursor_after_widening", "lines": ["aſb x"], "keys": "gUiw"},
    {"name": "cc_g_question_multibyte", "lines": ["ßa é"], "keys": "g??"},
    {"name": "cc_visual_U_sharp_s", "lines": ["ßǆ x"], "keys": "vlU"},
    {"name": "cc_visual_u_titlecase", "lines": ["ǅẞ x"], "keys": "vlu"},
    {"name": "cc_visual_tilde_mixed", "lines": ["ßǅ x"], "keys": "vl~"},
    {"name": "cc_visual_block_U", "lines": ["ßa", "ǆb"], "keys": "<C-v>jlU"},
    {"name": "cc_sub_upper_sharp_s", "lines": ["straße"], "keys": ":s/.*/\\U&/<CR>"},
    {"name": "cc_sub_upper_one_sharp_s", "lines": ["ßa"], "keys": ":s/ß/\\u&/<CR>"},
    {"name": "cc_sub_lower_capital_sharp_s", "lines": ["ẞA"], "keys": ":s/.*/\\L&/<CR>"},
    {"name": "cc_search_ic_no_sharp_s_fold", "lines": ["ß x ẞ"], "keys": ":set ic<CR>/ẞ<CR>x"},
    {"name": "cc_search_ic_kelvin", "lines": ["a K"], "keys": ":set ic<CR>/k<CR>x"},
    {"name": "sc_mb_dw_mixed", "lines": ["가나 abc 다라"], "keys": "wdw"},
    {"name": "sc_mb_cw_hangul", "lines": ["가나 다라"], "keys": "cwX<Esc>"},
    {"name": "sc_mb_yl_p_hangul", "lines": ["가나"], "keys": "ylp"},
    {"name": "sc_mb_j_keeps_column_by_char", "lines": ["가나다", "abcdef"], "keys": "llj"},
    {"name": "sc_mb_k_from_ascii_to_hangul", "lines": ["가나다", "abcdef"], "keys": "j$k"},
    {"name": "sc_mb_emoji_x", "lines": ["a😀b"], "keys": "l2x"},
    {"name": "sc_mb_emoji_e", "lines": ["😀😀 x"], "keys": "ex"},
    {"name": "sc_mb_search_hangul", "lines": ["abc 다라 다"], "keys": "/다<CR>nx"},
    {"name": "sc_mb_star_hangul", "lines": ["가나 x 가나"], "keys": "*x"},
    {"name": "sc_mb_sub_hangul", "lines": ["가나다"], "keys": ":s/나/X/<CR>"},
    {"name": "sc_mb_visual_block_hangul", "lines": ["가나다", "라마바"], "keys": "l<C-v>jd"},
    {"name": "sc_mb_J_hangul", "lines": ["가나", "다라"], "keys": "J"},
    {"name": "sc_mb_insert_hangul", "lines": ["ab"], "keys": "a가나<Esc>x"},
    {"name": "sc_mb_A_then_BS", "lines": ["가나"], "keys": "A다<BS><Esc>"},
    {"name": "sc_mb_di_paren_hangul", "lines": ["(가나)"], "keys": "ldi("},
    {"name": "sc_mb_percent_over_hangul", "lines": ["(가나)"], "keys": "%x"},
    #     empty and tiny buffers ------------------------------------------------------------------------
    {"name": "sc_edge_empty_x", "lines": [""], "keys": "x"},
    {"name": "sc_edge_empty_X", "lines": [""], "keys": "X"},
    {"name": "sc_edge_empty_dd", "lines": [""], "keys": "dd"},
    {"name": "sc_edge_empty_D", "lines": [""], "keys": "D"},
    {"name": "sc_edge_empty_yy_p", "lines": [""], "keys": "yyp"},
    {"name": "sc_edge_empty_J", "lines": [""], "keys": "J"},
    {"name": "sc_edge_empty_tilde", "lines": [""], "keys": "~"},
    {"name": "sc_edge_empty_r", "lines": [""], "keys": "rx"},
    {"name": "sc_edge_empty_dw", "lines": [""], "keys": "dw"},
    {"name": "sc_edge_empty_cw", "lines": [""], "keys": "cwX<Esc>"},
    {"name": "sc_edge_empty_diw", "lines": [""], "keys": "diw"},
    {"name": "sc_edge_empty_shift", "lines": [""], "keys": ">>"},
    {"name": "sc_edge_empty_cc", "lines": [""], "keys": "ccX<Esc>"},
    {"name": "sc_edge_empty_o", "lines": [""], "keys": "oX<Esc>"},
    {"name": "sc_edge_empty_O", "lines": [""], "keys": "OX<Esc>"},
    {"name": "sc_edge_empty_A", "lines": [""], "keys": "AX<Esc>"},
    {"name": "sc_edge_empty_s", "lines": [""], "keys": "sX<Esc>"},
    {"name": "sc_edge_empty_vd", "lines": [""], "keys": "vd"},
    {"name": "sc_edge_empty_Vd", "lines": [""], "keys": "Vd"},
    {"name": "sc_edge_empty_Vy", "lines": [""], "keys": "Vyp"},
    {"name": "sc_edge_empty_G_gg", "lines": [""], "keys": "Ggg"},
    {"name": "sc_edge_empty_count_j", "lines": [""], "keys": "5j"},
    {"name": "sc_edge_empty_search", "lines": [""], "keys": "/a<CR>"},
    {"name": "sc_edge_empty_star", "lines": [""], "keys": "*"},
    {"name": "sc_edge_empty_percent", "lines": [""], "keys": "%"},
    {"name": "sc_edge_empty_mark", "lines": [""], "keys": "ma'a"},
    {"name": "sc_edge_empty_ex_delete", "lines": [""], "keys": ":d<CR>"},
    {"name": "sc_edge_empty_ex_sub", "lines": [""], "keys": ":s/a/b/<CR>"},
    {"name": "sc_edge_empty_ctrl_a", "lines": [""], "keys": "<C-a>"},
    {"name": "sc_edge_empty_dot_no_change", "lines": [""], "keys": "."},
    {"name": "sc_edge_empty_u", "lines": [""], "keys": "u"},
    {"name": "sc_edge_one_char_x_twice", "lines": ["a"], "keys": "xx"},
    {"name": "sc_edge_one_char_dd", "lines": ["a"], "keys": "dd"},
    {"name": "sc_edge_one_char_w", "lines": ["a"], "keys": "wx"},
    {"name": "sc_edge_one_char_e", "lines": ["a"], "keys": "ex"},
    {"name": "sc_edge_one_char_diw", "lines": ["a"], "keys": "diw"},
    {"name": "sc_edge_one_char_daw", "lines": ["a"], "keys": "daw"},
    {"name": "sc_edge_one_char_p", "lines": ["a"], "keys": "ylp"},
    {"name": "sc_edge_one_char_J", "lines": ["a"], "keys": "J"},
    {"name": "sc_edge_blank_caret", "lines": ["   "], "keys": "^x"},
    {"name": "sc_edge_blank_w", "lines": ["   "], "keys": "wx"},
    {"name": "sc_edge_blank_dw", "lines": ["   "], "keys": "ldw"},
    {"name": "sc_edge_blank_diw", "lines": ["   "], "keys": "ldiw"},
    {"name": "sc_edge_blank_daw", "lines": ["   "], "keys": "daw"},
    {"name": "sc_edge_blank_cc", "lines": ["   "], "keys": "ccX<Esc>"},
    {"name": "sc_edge_blank_lines_dap", "lines": ["", "", ""], "keys": "jdap"},
    {"name": "sc_edge_blank_lines_dip", "lines": ["", "", ""], "keys": "jdip"},
    {"name": "sc_edge_blank_lines_J", "lines": ["", "", ""], "keys": "3J"},
    {"name": "sc_edge_blank_lines_brace", "lines": ["", "", ""], "keys": "j}x"},
    #     :NORMAL / :G NORMAL (#689, #852, #853) and gQ (#806, #807): `:normal` keys take the typed-key path
    #     (session-resolved `*`/`#`/`H`/`L`/`&`), the whole run is one undo step, and `:g` marks follow their
    #     lines and drop deleted ones.
    {"name": "normal_star_resolved", "lines": ["foo bar", "baz foo"], "keys": ":normal *x<CR>"},
    {"name": "normal_hash_resolved", "lines": ["foo bar", "baz foo"], "keys": "j$:normal #x<CR>"},
    {"name": "normal_L_resolved", "lines": ["one", "two"], "keys": ":normal Lx<CR>"},
    {"name": "normal_H_resolved", "lines": ["one", "two"], "keys": "j:normal Hx<CR>"},
    {"name": "normal_amp_resolved", "lines": ["aa", "aa"], "keys": ":s/a/b/<CR>j:normal &<CR>"},
    {"name": "normal_one_undo_step", "lines": ["abc def"], "keys": ":normal xwx<CR>u", "setup": UNDO_SETUP},
    {"name": "normal_range_one_undo_step", "lines": ["a", "b", "c"], "keys": ":%normal A;<CR>u", "setup": UNDO_SETUP},
    {"name": "normal_range_undo_redo", "lines": ["a", "b", "c"], "keys": ":%normal A;<CR>u<C-r>", "setup": UNDO_SETUP},
    {"name": "g_normal_one_undo_step", "lines": ["ax", "bx", "c"], "keys": ":g/x/normal A;<CR>u", "setup": UNDO_SETUP},
    {"name": "g_normal_yank_to_end", "lines": ["a x", "b", "c x", "d"], "keys": ":g/x/normal yyGp<CR>"},
    {"name": "g_normal_deletes_marked_line", "lines": ["a", "a", "b", "c"], "keys": ":g/a/normal jdd<CR>"},
    {"name": "g_normal_moves_marked_line", "lines": ["x1", "y", "x2", "z"], "keys": ":g/x/normal ddp<CR>"},
    {"name": "g_normal_deletes_line_above", "lines": ["a", "x", "b", "x"], "keys": ":g/x/normal kdd<CR>"},
    {"name": "gQ_drops_count", "lines": ["abcdef"], "keys": "3gQvi<CR>x"},
    {"name": "gQ_under_operator_cancels", "lines": ["foo bar baz"], "keys": "dgQvi<CR>w"},
    {"name": "gQ_from_insert_ctrl_o", "lines": ["abcd"], "keys": "i<C-o>gQvi<CR>Z<Esc>"},
    {"name": "gQ_normal_runs_in_normal_mode", "lines": ["foo bar", "baz"], "keys": "gQnormal x<CR>vi<CR>"},
    #     :s REPLACEMENT LINE BREAKS (#698): `\r` splits the line and `\n` is a NUL; the cursor follows the
    #     last replacement down to its line, at the first non-blank. (A literal CR typed with CTRL-V needs
    #     the command-line CTRL-V, which ruse lacks; the expander is unit-tested for it.)
    {"name": "sub_rep_cr_splits_line", "lines": ["a,b"], "keys": ":s/,/\\r/<CR>"},
    {"name": "sub_rep_backslash_n_is_nul", "lines": ["a,b"], "keys": ":s/,/\\n/<CR>"},
    {"name": "sub_rep_cr_cursor_on_new_line", "lines": ["x", "  abc"], "keys": ":%s/b/1\\r  2/<CR>"},
    {"name": "sub_rep_cr_insert_after", "lines": ["x", "abc"], "keys": ":%s/b/1\\r2/<CR>iZ<Esc>"},
    {"name": "sub_rep_cr_global", "lines": ["a,b,c", "d,e"], "keys": ":%s/,/\\r/g<CR>"},
    {"name": "sub_rep_double_backslash_r", "lines": ["a,b"], "keys": ":s/,/\\\\r/<CR>"},
    {"name": "sub_cursor_first_non_blank", "lines": ["  abc"], "keys": "$:s/b/X/<CR>"},
    #     :s / :g PATTERN LINE BREAKS (#715): `\n` matches across lines, the joined line is searched again
    #     inside the range, the last line's break matches and stays, and `:s/\n//` is a join.
    {"name": "sub_pat_nl_joins_all", "lines": ["a", "b", "c"], "keys": ":%s/\\n/,/<CR>"},
    {"name": "sub_pat_nl_one_line_range", "lines": ["a", "b", "c"], "keys": ":1s/\\n/,/<CR>"},
    {"name": "sub_pat_nl_two_line_range", "lines": ["a", "b", "c"], "keys": ":1,2s/\\n/,/<CR>"},
    {"name": "sub_pat_nl_inside", "lines": ["a", "b", "c", "d"], "keys": ":%s/b\\nc/X/<CR>"},
    {"name": "sub_pat_nl_global_joined", "lines": ["ab", "ab", "c"], "keys": ":%s/b\\na/X/g<CR>"},
    {"name": "sub_pat_nl_two_breaks", "lines": ["a", "", "", "b"], "keys": ":%s/\\n\\n/X/<CR>"},
    {"name": "sub_pat_nl_alternation", "lines": ["a b", "c"], "keys": ":%s/ \\|\\n/_/g<CR>"},
    {"name": "sub_pat_nl_zs", "lines": ["a", "b"], "keys": ":%s/\\n\\zs/X/<CR>"},
    {"name": "sub_pat_nl_to_two_crs", "lines": ["a", "b", "c"], "keys": ":%s/\\n/\\r\\r/<CR>"},
    {"name": "sub_pat_nl_amp", "lines": ["a", "b"], "keys": ":%s/\\n/&&/<CR>"},
    {"name": "sub_pat_nl_star_global", "lines": ["aa", "b"], "keys": ":%s/a*\\n/X/g<CR>"},
    {"name": "sub_pat_nl_empty_is_join", "lines": ["ab", "cd", "ef"], "keys": ":%s/\\n//<CR>"},
    {"name": "sub_pat_nl_empty_range_join", "lines": ["ab", "cd", "ef"], "keys": ":1,2s/\\n//<CR>"},
    {"name": "sub_pat_nl_empty_last_line", "lines": ["ab", "cd"], "keys": ":2s/\\n//<CR>"},
    {"name": "g_pat_nl_marks_start_line", "lines": ["a", "b", "c"], "keys": ":g/a\\nb/s/^/#/<CR>"},
    {"name": "v_pat_nl_marks", "lines": ["x", "x", "x", "y"], "keys": ":v/x\\nx/s/^/#/<CR>"},
    {"name": "g_sub_nl_skips_joined_line", "lines": ["a", "a", "b"], "keys": ":g/a/s/\\n/-/<CR>"},
    #     DELIMITERS INSIDE [...] (#803): the pattern ends where Vim's skip_regexp says.
    {"name": "search_slash_in_collection", "lines": ["x/y"], "keys": "/[/]<CR>x"},
    {"name": "search_slash_in_collection_offset", "lines": ["x/y"], "keys": "/[/]/e<CR>x"},
    {"name": "sub_slash_in_collection", "lines": ["a/b"], "keys": ":s/[/]/x/<CR>"},
    {"name": "sub_slash_in_collection_very_magic", "lines": ["a/b"], "keys": ":s/\\v[/]/x/<CR>"},
    {"name": "sub_unclosed_collection_takes_the_rest", "lines": ["a[b"], "keys": ":s/[/x/<CR>"},
    {"name": "sub_bracket_slash_literal", "lines": ["a[/b"], "keys": ":s/[/]/x/<CR>"},
    {"name": "sub_bar_in_collection", "lines": ["a|b"], "keys": ":s/[|]/x/<CR>"},
    {"name": "g_slash_in_collection", "lines": ["a/b", "c"], "keys": ":g/[/]/s/^/#/<CR>"},
    #     EMPTY PATTERN REUSE (#804): `:g//`, the `s//` inside `:g`, and `:s` setting the last pattern.
    {"name": "g_sub_empty_uses_g_pattern", "lines": ["aa", "b"], "keys": ":g/a/s//X/<CR>"},
    {"name": "g_empty_uses_last_search", "lines": ["a", "b"], "keys": "/a<CR>:g//s/^/#/<CR>"},
    {"name": "g_empty_no_previous_pattern", "lines": ["a", "b"], "keys": ":g//d<CR>"},
    {"name": "v_empty_no_previous_pattern", "lines": ["a", "b"], "keys": ":v//d<CR>"},
    {"name": "g_empty_uses_last_sub_pattern", "lines": ["ab", "b"], "keys": ":s/a/X/<CR>:g//d<CR>"},
    {"name": "sub_sets_last_search", "lines": ["ab", "ba"], "keys": ":s/a/X/<CR>jn"},
    {"name": "g_sets_last_search", "lines": ["ab", "ba"], "keys": ":g/b/s/a/X/<CR>ggn"},
    #     :sort ARGUMENTS AND KEYS (#719), as nvim's ex_sort reads and compares them.
    {"name": "sort_unclosed_collection_e654", "lines": ["b", "a"], "keys": ":sort /[/<CR>"},
    {"name": "sort_unclosed_pattern_e654", "lines": ["b", "a"], "keys": ":sort /a<CR>"},
    {"name": "sort_bad_regex", "lines": ["b", "a"], "keys": ":sort /\\(/<CR>"},
    {"name": "sort_flag_after_pattern", "lines": ["x9", "x10", "x1"], "keys": ":sort /x/ n<CR>"},
    {"name": "sort_flag_before_pattern", "lines": ["x9", "x10", "x1"], "keys": ":sort n /x/<CR>"},
    {"name": "sort_numeric_no_number_first", "lines": ["b", "-5", "a", "3"], "keys": ":sort n<CR>"},
    {"name": "sort_numeric_overflow_saturates", "lines": ["99999999999999999999", "5", "-99999999999999999999"], "keys": ":sort n<CR>"},
    {"name": "sort_reverse_unique_ignore_case", "lines": ["a", "A", "b"], "keys": ":sort! iu<CR>"},
    {"name": "sort_unique_ignore_case", "lines": ["a", "A", "b"], "keys": ":sort iu<CR>"},
    {"name": "sort_unique_compares_lines", "lines": ["1", "01", "2"], "keys": ":sort nu<CR>"},
    {"name": "sort_bad_flag_e475", "lines": ["b", "a"], "keys": ":sort z<CR>"},
    {"name": "sort_two_formats_e474", "lines": ["b", "a"], "keys": ":sort nx<CR>"},
    {"name": "sort_second_pattern_e475", "lines": ["b", "a"], "keys": ":sort /a/ /b/<CR>"},
    {"name": "sort_empty_pattern_no_previous", "lines": ["b", "a"], "keys": ":sort //<CR>"},
    {"name": "sort_empty_pattern_last_search", "lines": ["xb", "ya"], "keys": "/x\\|y<CR>:sort //<CR>"},
    {"name": "sort_space_delimiter", "lines": ["b x", "a y"], "keys": ":sort / /<CR>"},
    {"name": "sort_comment", "lines": ["b", "a"], "keys": ":sort \" comment<CR>"},
    {"name": "sort_pattern_ignorecase", "lines": ["Bx", "ax"], "keys": ":set ic<CR>:sort /X/<CR>"},
    {"name": "sort_cursor_first_non_blank", "lines": ["  b", "a"], "keys": "j:sort<CR>"},
    {"name": "sortn_is_not_sort", "lines": ["b", "a"], "keys": ":sortn<CR>"},
    #     FAILED COMMANDS (#1085): a failed motion cancels its operator and aborts a macro replay or a `:normal`
    #     payload; keys typed after it still run.
    {"name": "nl_fail_count_dollar_last_line", "lines": ["ab", "cd"], "keys": "j2$"},
    {"name": "nl_fail_d_count_dollar_last_line", "lines": ["ab", "cd"], "keys": "jd2$"},
    {"name": "nl_fail_count_e_moves_then_fails", "lines": ["ab cd ef"], "keys": "5ex"},
    {"name": "nl_fail_visual_l_at_eol", "lines": ["ab"], "keys": "v$ld"},
    {"name": "nl_mac_failed_w_aborts", "lines": ["$wx", "abc"], "keys": "\"ay$j@a"},
    {"name": "nl_normal_failed_motion_stops", "lines": ["abc", "abc"], "keys": ":2normal $wx<CR>"},
    {"name": "nl_normal_failure_per_line", "lines": ["ax", "bc"], "keys": ":%normal fcx<CR>"},
    {"name": "nl_g_normal_failure_skips_rest", "lines": ["x1", "y", "x2"], "keys": ":g/x/normal jdd<CR>"},
    {"name": "nl_g_normal_failure_per_pass", "lines": ["xa", "xb", "y"], "keys": ":g/x/normal fbx<CR>"},
    #     WANTED COLUMN (#1090): `j`/`k` keep the screen column over tabs and wide chars.
    {"name": "nl_mb_k_screen_col", "lines": ["abcdef", "가나다"], "keys": "4lj"},
    {"name": "nl_mb_k_back_to_ascii", "lines": ["abcdef", "가나다"], "keys": "4ljk"},
    {"name": "nl_tab_j_screen_col", "lines": ["\tx", "abcdefghij"], "keys": "lj", "setup": "set ts=4"},
    {"name": "nl_tab_k_past_short", "lines": ["\tx", "abcdefghij"], "keys": "j3lk", "setup": "set ts=4"},
    {"name": "nl_tab_k_onto_tab_keeps_want", "lines": ["\tx", "abcdefghij"], "keys": "j2lkj", "setup": "set ts=4"},
    {"name": "nl_mb_block_j", "lines": ["가나다", "abcdef"], "keys": "l<C-v>jd"},
    #     PARAGRAPH AND WORD OBJECTS (#1089): counts, failures and where a failed object leaves the cursor.
    {"name": "nl_d2ap", "lines": ["a", "", "b"], "keys": "d2ap"},
    {"name": "nl_d3ap_fails", "lines": ["a", "", "b"], "keys": "d3ap"},
    {"name": "nl_dap_on_blank_mid", "lines": ["a", "", "", "b"], "keys": "jdap"},
    {"name": "nl_d2ip", "lines": ["a", "", "b"], "keys": "d2ip"},
    {"name": "nl_daw_blank_mid", "lines": ["   "], "keys": "ldaw"},
    {"name": "nl_daW_blank", "lines": ["   "], "keys": "daW"},
    {"name": "nl_dap_white_line_is_blank", "lines": ["a", "  ", "b"], "keys": "dap"},
    #     REGISTERS (#1091): a named delete fills "1; a recording updates the unnamed register naming it.
    {"name": "nl_reg_named_small_delete_skips_minus", "lines": ["abc def"], "keys": "\"adw\"-p"},
    {"name": "nl_reg_named_delete_lands_in_1", "lines": ["1", "2", "3"], "keys": "\"add\"1p"},
    {"name": "nl_mac_empty_recording_unnamed_elsewhere", "lines": ["abc"], "keys": "yyqaqp"},
    {"name": "nl_mac_recording_updates_unnamed", "lines": ["abc"], "keys": "\"ayyqalqp"},
    #     MARKS (#1088): `^, :mark/:k, uppercase marks, `> under an operator, `[`]` after a put, `` after `/`.
    {"name": "nl_mark_last_insert_line", "lines": ["abc", "  def"], "keys": "jAxy<Esc>gg'^x"},
    {"name": "nl_mark_d_to_last_insert", "lines": ["abc def"], "keys": "wix<Esc>0d`^"},
    {"name": "nl_mark_ex_mark_current_line", "lines": ["a", "b", "c"], "keys": "j:mark b<CR>gg'bx"},
    {"name": "nl_mark_ex_k_space", "lines": ["a", "b", "c"], "keys": "j:k b<CR>gg'bx"},
    {"name": "nl_mark_ex_mark_no_argument", "lines": ["a", "b"], "keys": ":mark<CR>"},
    {"name": "nl_mark_ex_k_no_argument", "lines": ["a", "b"], "keys": ":k<CR>"},
    {"name": "nl_mark_ex_mark_trailing", "lines": ["a", "b"], "keys": ":mark ab<CR>"},
    {"name": "nl_mark_uppercase_in_ex_range", "lines": ["a", "b", "c"], "keys": "jmAgg:'Ad<CR>"},
    {"name": "nl_mark_uppercase_backtick", "lines": ["abc", "def"], "keys": "jlmBgg`Bx"},
    {"name": "nl_mark_y_to_visual_end", "lines": ["abc def ghi"], "keys": "wviw<Esc>0y`>P"},
    {"name": "nl_mark_change_start_put_below_last", "lines": ["abc", "def"], "keys": "yyjp`]x"},
    {"name": "nl_mark_backtick_back_after_search", "lines": ["abc", "def x"], "keys": "l/x<CR>``x"},
    #     EXPRESSION REGISTER AND DOT (#808).
    {"name": "nl_expr_insert_dot", "lines": ["abc"], "keys": "i<C-r>=1+1<CR><Esc>."},
    {"name": "nl_expr_put_dot", "lines": ["abc"], "keys": "\"=1+1<CR>p."},
    #     VISUAL PUT COUNT (#1093).
    {"name": "nl_vput_count_charwise", "lines": ["foo bar"], "keys": "yiwwviw2p"},
    {"name": "nl_vput_count_linewise", "lines": ["foo", "bar"], "keys": "yyjV2p"},
    {"name": "nl_vput_count_P", "lines": ["foo bar"], "keys": "yiwwviw3P"},
    {"name": "nl_vput_count_line_reg_over_chars", "lines": ["foo", "bar baz"], "keys": "yyjwviw2p"},
    #     SILENT FAILURES (#1111): `u` with nothing to undo, `J` on the last line, `r` past the line end and
    #     an unknown key beep and abort a macro or a `:normal`. The undo fixtures clear the undo entry the
    #     harness's `nvim_buf_set_lines` makes, so nvim starts with no history, as ruse does. `x` on an empty
    #     line and `<Esc>` in Normal do not fail.
    {"name": "nl_fail_u_nothing_normal", "lines": ["abc", "def"], "keys": ":normal ujx<CR>",
     "setup": 'set ul=-1 | exe "normal! a \\<BS>\\<Esc>" | set ul=1000'},
    {"name": "nl_mac_u_nothing_aborts", "lines": ["ujx", "abc", "def"], "keys": "\"ay$j@a",
     "setup": 'set ul=-1 | exe "normal! a \\<BS>\\<Esc>" | set ul=1000'},
    {"name": "nl_fail_u_count_stops", "lines": ["abc"], "keys": "x:normal 2uiy<CR>",
     "setup": 'set ul=-1 | exe "normal! a \\<BS>\\<Esc>" | set ul=1000'},
    {"name": "nl_fail_J_last_line_normal", "lines": ["ab"], "keys": ":normal Jx<CR>"},
    {"name": "nl_mac_J_last_line_aborts", "lines": ["Jx", "ab"], "keys": "\"ay$j@a"},
    {"name": "nl_J_count_past_end_clamps", "lines": ["ab", "cd"], "keys": ":normal 3Jx<CR>"},
    {"name": "nl_fail_gJ_last_line_normal", "lines": ["ab"], "keys": ":normal gJx<CR>"},
    {"name": "nl_fail_vJ_one_last_line", "lines": ["ab"], "keys": ":normal VJx<CR>"},
    {"name": "nl_fail_r_past_end_normal", "lines": ["ab"], "keys": ":normal 3rzx<CR>"},
    {"name": "nl_mac_r_past_end_aborts", "lines": ["3rzx", "ab"], "keys": "\"ay$j@a"},
    {"name": "nl_x_empty_line_goes_on", "lines": ["", "ab"], "keys": ":normal xjx<CR>"},
    {"name": "nl_esc_in_normal_goes_on", "lines": ["ab", "cd"], "keys": ":let @a=\"\\<Esc>jx\"<CR>@a"},
    {"name": "nl_op_esc_goes_on", "lines": ["ab", "cd"], "keys": ":let @a=\"d\\<Esc>jx\"<CR>@a"},
    {"name": "nl_fail_unknown_Z_normal", "lines": ["ab", "cd"], "keys": ":normal Zqjx<CR>"},
    {"name": "nl_fail_unknown_g_normal", "lines": ["ab", "cd"], "keys": ":normal gKjx<CR>"},
    {"name": "nl_fail_unknown_z_normal", "lines": ["ab", "cd"], "keys": ":normal zZjx<CR>"},
    {"name": "nl_fail_unknown_after_count", "lines": ["ab", "cd"], "keys": ":normal 3Zqjx<CR>"},
    {"name": "nl_fail_unknown_after_register", "lines": ["ab", "cd"], "keys": ":normal \"aZqjx<CR>"},
    {"name": "nl_fail_unknown_in_visual", "lines": ["ab", "cd"], "keys": ":normal vZqjx<CR>"},
    {"name": "nl_fail_unknown_motion_cancels_op", "lines": ["ab", "cd"], "keys": ":normal dZjx<CR>"},
    {"name": "nl_mac_unknown_key_aborts", "lines": ["Zqx", "ab"], "keys": "\"ay$j@a"},
    {"name": "nl_vaw_blank_line", "lines": ["   "], "keys": "vawd"},
    {"name": "nl_vaw_blank_line_mid", "lines": ["   "], "keys": "lvawd"},
    {"name": "nl_vaw_blank_line_then_word", "lines": ["   ", "ab"], "keys": "vawd"},
    #     COMMAND-LINE EDITOR (#612, #811): the caret moves and edits mid-line; `c_CTRL-V` types a literal key;
    #     `<BS>`/`<Del>` on an empty line abandon it. Each line's effect shows in the buffer.
    {"name": "cl_left_then_type", "lines": ["abc"], "keys": ":normal ib<Left>a<CR>"},
    {"name": "cl_home_end_edit", "lines": ["a", "b"], "keys": ":x<Home>d<End><BS><CR>"},
    {"name": "cl_ctrl_b_ctrl_e", "lines": ["aa", "aa"], "keys": ":s/a/b/g<C-b>%<C-e><CR>"},
    {"name": "cl_s_left_word", "lines": ["x"], "keys": ":normal iab cd<S-Left>X<CR>"},
    {"name": "cl_c_left_word", "lines": ["x"], "keys": ":normal iab  cd<C-Left>X<CR>"},
    {"name": "cl_s_right_word", "lines": ["x"], "keys": ":normal iab cd<Home><S-Right><S-Right>X<CR>"},
    {"name": "cl_c_right_word", "lines": ["x"], "keys": ":normal iab cd<Home><C-Right>X<CR>"},
    {"name": "cl_del_under_caret", "lines": ["x"], "keys": ":normal iaXb<Left><Left><Del><CR>"},
    {"name": "cl_del_at_end", "lines": ["x"], "keys": ":normal iabX<Del><CR>"},
    {"name": "cl_bs_mid_line", "lines": ["x"], "keys": ":normal iaXb<Left><BS><CR>"},
    {"name": "cl_bs_at_start_keeps_line", "lines": ["a", "b"], "keys": ":d<Home><BS><CR>"},
    {"name": "cl_ctrl_h_mid_line", "lines": ["x"], "keys": ":normal iaXb<Left><C-h><CR>"},
    {"name": "cl_ctrl_u_keeps_after_caret", "lines": ["x"], "keys": ":abcnormal ia<Home><Right><Right><Right><C-u><CR>"},
    {"name": "cl_ctrl_w_before_caret", "lines": ["x"], "keys": ":normal ia.b<Left><C-w><CR>"},
    {"name": "cl_insert_overstrike", "lines": ["x"], "keys": ":normal iabc<Left><Left><Insert>XYZ<CR>"},
    {"name": "cl_insert_toggles_back", "lines": ["x"], "keys": ":normal iac<Left><Insert><Insert>b<CR>"},
    {"name": "cl_ctrl_v_literal_cr", "lines": ["abc"], "keys": ":normal :s/a/X/<C-v><CR><CR>"},
    {"name": "cl_ctrl_q_literal_cr", "lines": ["abc"], "keys": ":normal :s/b/X/<C-q><CR><CR>"},
    {"name": "cl_ctrl_v_decimal", "lines": ["x"], "keys": ":normal i<C-v>065<CR>"},
    {"name": "cl_ctrl_v_unicode", "lines": ["x"], "keys": ":normal i<C-v>u00e9<CR>"},
    {"name": "cl_ctrl_v_code_then_key", "lines": ["x"], "keys": ":normal ia<C-v>u62<Left>c<CR>"},
    {"name": "cl_ctrl_k_digraph", "lines": ["x"], "keys": ":normal i<C-k>e:<CR>"},
    {"name": "cl_ctrl_r_register", "lines": ["abc"], "keys": "yiw:normal A<C-r>\"<CR>"},
    {"name": "cl_ctrl_r_mid_line", "lines": ["abc"], "keys": "yiw:normal Az<Left><C-r>0<CR>"},
    {"name": "cl_ctrl_r_ctrl_r_register", "lines": ["abc"], "keys": "yiw:normal A<C-r><C-r>0<CR>"},
    {"name": "cl_ctrl_r_ctrl_l_line", "lines": ["abc"], "keys": ":normal A<C-r><C-l><CR>"},
    {"name": "cl_ctrl_r_empty_register", "lines": ["abc"], "keys": ":normal Ax<C-r>q<CR>"},
    {"name": "cl_s_up_unfiltered_history", "lines": ["aa"], "keys": ":s/a/b/<CR>u:x<S-Up><CR>"},
    {"name": "cl_page_up_unfiltered_history", "lines": ["aa"], "keys": ":s/a/b/<CR>u:x<PageUp><CR>"},
    {"name": "cl_up_filters_by_text_before_caret", "lines": ["aa", "bb"], "keys": ":s/a/b/<CR>:d<CR>u:sx<Left><Up><CR>"},
    {"name": "cl_bs_on_empty_cancels", "lines": ["abc"], "keys": ":<BS>x"},
    {"name": "cl_bs_on_empty_search_cancels", "lines": ["abc"], "keys": "/<BS>x"},
    {"name": "cl_bs_twice_cancels", "lines": ["abc"], "keys": ":a<BS><BS>x"},
    {"name": "cl_ctrl_h_on_empty_cancels", "lines": ["abc"], "keys": ":<C-h>x"},
    {"name": "cl_del_on_empty_cancels", "lines": ["abc"], "keys": ":<Del>x"},
    {"name": "cl_ctrl_w_on_empty_stays", "lines": ["abc"], "keys": ":<C-w>d<CR>"},
    {"name": "cl_ctrl_u_on_empty_stays", "lines": ["abc", "d"], "keys": ":<C-u>d<CR>"},
    #     i_CTRL-O :normal (#1097): the keys run in Normal mode, then Insert resumes where Vim's `edit()` puts it.
    {"name": "ictrlo_normal_runs_in_normal", "lines": ["abc"], "keys": "i<C-o>:normal x<CR>Z<Esc>"},
    {"name": "ictrlo_normal_at_eol", "lines": ["abc"], "keys": "A<C-o>:normal x<CR>Z<Esc>"},
    {"name": "ictrlo_normal_mid_line", "lines": ["abc"], "keys": "la<C-o>:normal x<CR>Z<Esc>"},
    {"name": "ictrlo_normal_motion", "lines": ["abc"], "keys": "A<C-o>:normal 0<CR>Z<Esc>"},
    {"name": "ictrlo_normal_insert_keys", "lines": ["abc"], "keys": "i<C-o>:normal ix<CR>Z<Esc>"},
    #     EX COMMAND WORDS (#609): every abbreviation nvim accepts resolves through one table; modifiers come
    #     first; a word nvim gives to another command is not ruse's.
    {"name": "exw_del", "lines": ["a", "b"], "keys": ":del<CR>"},
    {"name": "exw_dele", "lines": ["a", "b", "c"], "keys": ":2dele<CR>"},
    {"name": "exw_su", "lines": ["aa"], "keys": ":su/a/b/<CR>"},
    {"name": "exw_substitute_global", "lines": ["aa"], "keys": ":substitute/a/b/g<CR>"},
    {"name": "exw_ya_put", "lines": ["a", "b"], "keys": ":ya<CR>:pu<CR>"},
    {"name": "exw_jo", "lines": ["a", "b"], "keys": ":jo<CR>"},
    {"name": "exw_mo", "lines": ["a", "b"], "keys": ":mo $<CR>"},
    {"name": "exw_cop", "lines": ["a", "b"], "keys": ":cop $<CR>"},
    {"name": "exw_norm", "lines": ["abc"], "keys": ":norm x<CR>"},
    {"name": "exw_glo_bang", "lines": ["a", "b", "a"], "keys": ":global!/a/s/b/c/<CR>"},
    {"name": "exw_sor", "lines": ["b", "a"], "keys": ":%sor<CR>"},
    {"name": "exw_silent_hides_nothing_of_the_edit", "lines": ["a", "b"], "keys": ":silent d<CR>"},
    {"name": "exw_sil_range_after_modifier", "lines": ["a", "b", "c"], "keys": ":sil 2d<CR>"},
    {"name": "exw_silent_bang_continues_the_bar", "lines": ["aa"], "keys": ":silent! s/z/y/|s/a/b/<CR>"},
    {"name": "exw_silent_error_stops_the_bar", "lines": ["aa"], "keys": ":sil s/z/y/|s/a/b/<CR>"},
    {"name": "exw_silent_normal_owns_the_bar", "lines": ["abc"], "keys": ":silent normal x|x<CR>"},
    {"name": "exw_verbose_noautocmd", "lines": ["a", "b"], "keys": ":verb noa d<CR>"},
    {"name": "exw_unsilent", "lines": ["a", "b"], "keys": ":sil uns d<CR>"},
    {"name": "exw_s_flags_after_bar", "lines": ["aa"], "keys": ":s/a/b/ | s/b/c/<CR>"},
    {"name": "exw_s_n_flag_before_bar", "lines": ["aa"], "keys": ":s/a/n/|normal x<CR>"},
    #     EX LEFTOVERS (#1104): `:sort` number formats, `:s` flags and count, `:g` with an inner `:s`.
    {"name": "exl_sort_hex", "lines": ["x0x1F", "b", "0x0a", "-0x3", "0xff", "zz"], "keys": ":sort x<CR>"},
    {"name": "exl_sort_hex_reverse", "lines": ["0x10", "9", "x"], "keys": ":sort! x<CR>"},
    {"name": "exl_sort_hex_unique_whole_lines", "lines": ["0x10 a", "0x10 b", "q", "0xA"], "keys": ":sort xu<CR>"},
    {"name": "exl_sort_binary", "lines": ["0b101", "0b11", "b", "-0b1", "10"], "keys": ":sort b<CR>"},
    {"name": "exl_sort_octal", "lines": ["017", "8", "07", "a", "-01", "9"], "keys": ":sort o<CR>"},
    {"name": "exl_sort_float", "lines": ["1.5", "a", "-2e1", "0.3", "b", "1e-1", "3"], "keys": ":sort f<CR>"},
    {"name": "exl_sort_float_prefixes", "lines": ["x", "1.5e", "inf", "-inf", "2", " ", "0x1p2", "0x.8", "+ 3"], "keys": ":sort f<CR>"},
    {"name": "exl_sort_float_and_n_fail", "lines": ["b", "a"], "keys": ":sort fn<CR>"},
    {"name": "exl_sort_pattern_n", "lines": ["a3 x", "b1 x", "c2"], "keys": ":sort /./ n<CR>"},
    {"name": "exl_s_unknown_flag", "lines": ["aa", "aa"], "keys": ":s/a/b/z<CR>"},
    {"name": "exl_s_flag_then_garbage", "lines": ["aa", "aa"], "keys": ":s/a/b/g2x<CR>"},
    {"name": "exl_s_count", "lines": ["aa", "aa", "aa"], "keys": ":s/a/b/ 2<CR>"},
    {"name": "exl_s_count_unspaced", "lines": ["aa", "aa", "aa"], "keys": ":s/a/b/2<CR>"},
    {"name": "exl_s_g_count", "lines": ["aa", "aa", "aa"], "keys": ":s/a/b/g 2<CR>"},
    {"name": "exl_s_count_from_range_end", "lines": ["aa", "aa", "aa"], "keys": ":2s/a/b/ 5<CR>"},
    {"name": "exl_s_count_zero", "lines": ["aa", "aa"], "keys": ":s/a/b/ 0<CR>"},
    {"name": "exl_s_gg_toggles_off", "lines": ["aa"], "keys": ":s/a/b/gg<CR>"},
    {"name": "exl_s_comment_after_flags", "lines": ["aa"], "keys": ':s/a/b/g "note<CR>'},
    {"name": "exl_s_print_flag", "lines": ["aa", "aa"], "keys": ":s/a/b/p<CR>"},
    {"name": "exl_s_number_flag", "lines": ["aa", "aa"], "keys": ":s/a/b/#<CR>"},
    {"name": "exl_s_amp_keeps_g", "lines": ["aa", "aa", "aa"], "keys": ":s/a/b/g<CR>j:s/a/c/&<CR>"},
    {"name": "exl_s_amp_without_g", "lines": ["aa", "aa"], "keys": ":s/a/b/<CR>:s/a/c/&<CR>"},
    {"name": "exl_s_amp_not_first", "lines": ["aa", "aa"], "keys": ":s/a/b/<CR>j:s/a/c/g&<CR>"},
    {"name": "exl_g_inner_s_unmatched_sets_pattern", "lines": ["ab", "ab", "ab"], "keys": ":g/a/s/zzz/y/<CR>:s//Q/<CR>"},
    {"name": "exl_g_inner_s_unmatched_cursor", "lines": ["ab", "ab"], "keys": ":g/a/s/zzz/y/<CR>x"},
    {"name": "exl_g_no_match_sets_pattern", "lines": ["ab", "ab", "ab"], "keys": ":g/zz/s/b/y/<CR>:s//Q/<CR>"},
    #     EX LEFTOVERS (#609): the print family, `:z`, `:=`, `:put!`, `:retab`, the Insert starters and the
    #     `:keeppatterns`/`:keepjumps`/`:filter` modifiers. Only the cursor and buffer of a print are observed.
    {"name": "exl_print_range_cursor", "lines": ["a", "b", "c", "d"], "keys": ":2,3p<CR>"},
    {"name": "exl_print_count", "lines": ["a", "b", "c", "d"], "keys": ":2p 2<CR>"},
    {"name": "exl_print_count_and_flags", "lines": ["a", "b", "c", "d"], "keys": ":2p 2 l#<CR>"},
    {"name": "exl_print_trailing", "lines": ["a", "b"], "keys": ":p x<CR>"},
    {"name": "exl_print_zero_count", "lines": ["a", "b"], "keys": ":p 0<CR>"},
    {"name": "exl_print_keeps_column", "lines": ["abcd", "abcd"], "keys": "ll:2#<CR>"},
    {"name": "exl_list_cursor", "lines": ["a", "b\tc", "d"], "keys": ":2l<CR>"},
    {"name": "exl_number_abbrev", "lines": ["a", "b", "c"], "keys": ":%nu<CR>"},
    {"name": "exl_pl_is_unknown", "lines": ["a", "b", "c"], "keys": ":2pl<CR>"},
    {"name": "exl_P_is_unknown", "lines": ["a", "b"], "keys": ":P<CR>"},
    {"name": "exl_g_print_cursor", "lines": ["aa", "b", "aa", "c"], "keys": ":g/a/p<CR>"},
    {"name": "exl_g_number_cursor", "lines": ["aa", "b", "aa", "c"], "keys": ":g/a/nu<CR>"},
    {"name": "exl_z_addressed", "lines": ["a", "b", "c", "d"], "keys": ":2z<CR>"},
    {"name": "exl_z_from_cursor", "lines": ["abcd", "  bcd", "xyzw"], "keys": "ll:z<CR>"},
    {"name": "exl_z_minus", "lines": ["a", "b", "c", "d"], "keys": ":3z-<CR>"},
    {"name": "exl_z_minus_count", "lines": ["a", "b", "c", "d"], "keys": ":3z-2<CR>"},
    {"name": "exl_z_equal", "lines": ["a", "b", "c", "d", "e", "f", "g"], "keys": ":4z=1<CR>"},
    {"name": "exl_z_dot", "lines": ["a", "b", "c", "d"], "keys": ":2z.<CR>"},
    {"name": "exl_z_caret", "lines": ["a", "b", "c", "d"], "keys": "G:2z^<CR>"},
    {"name": "exl_z_plus", "lines": ["a", "b", "c", "d"], "keys": ":z+<CR>"},
    {"name": "exl_z_count", "lines": ["a", "b", "c", "d"], "keys": ":z 2<CR>"},
    {"name": "exl_z_zero", "lines": ["a", "b", "c"], "keys": "j:z 0<CR>"},
    {"name": "exl_z_number_flag", "lines": ["a", "  b", "c", "d"], "keys": "l:2z#<CR>"},
    {"name": "exl_z_other_word", "lines": ["a", "b", "c"], "keys": ":zx<CR>"},
    {"name": "exl_z_non_numeric", "lines": ["a", "b", "c"], "keys": ":z-x<CR>"},
    {"name": "exl_line_number_stays", "lines": ["abcd", "  bcd", "x"], "keys": "ll:2=<CR>"},
    {"name": "exl_put_bang", "lines": ["a", "b", "c"], "keys": "yy:2put!<CR>"},
    {"name": "exl_put_bang_current", "lines": ["a", "b", "c"], "keys": "jyy:put!<CR>"},
    {"name": "exl_put_bang_first_line", "lines": ["a", "b", "c"], "keys": "yy:put!<CR>"},
    {"name": "exl_put_bang_last", "lines": ["a", "b", "c"], "keys": "yy:$put!<CR>"},
    {"name": "exl_put_bang_register", "lines": ["a", "b", "c"], "keys": '"ayyj:put! a<CR>'},
    {"name": "exl_put_empty_register", "lines": ["a", "b", "c"], "keys": "yy:$put! a<CR>"},
    {"name": "exl_retab_bang_noet", "lines": ["\tab", "        cd", "    ef"], "keys": ":set noet<CR>:set ts=8<CR>:retab!<CR>", "setup": "set tabstop=4 expandtab"},
    {"name": "exl_retab_et", "lines": ["\tab", "        cd", "    ef"], "keys": ":set ts=8<CR>:retab<CR>", "setup": "set tabstop=4 expandtab"},
    {"name": "exl_retab_noet_new_ts", "lines": ["\tab", "        cd", "    ef"], "keys": ":set noet<CR>:set ts=8<CR>:retab 4<CR>", "setup": "set tabstop=4 expandtab"},
    {"name": "exl_retab_range_bang", "lines": ["\tab", "        cd", "    e\tf"], "keys": ":set noet<CR>:set ts=8<CR>:2retab! 4<CR>", "setup": "set tabstop=4 expandtab"},
    {"name": "exl_retab_mixed_run", "lines": ["  \t  ab"], "keys": ":set noet<CR>:set ts=8<CR>:retab!<CR>", "setup": "set tabstop=4 expandtab"},
    {"name": "exl_retab_mid_line", "lines": ["a  \t  ab"], "keys": ":set ts=8<CR>:retab 2<CR>", "setup": "set tabstop=4 expandtab"},
    {"name": "exl_retab_sets_tabstop", "lines": ["a\tb"], "keys": ":set noet<CR>:retab 8<CR>:retab! 4<CR>", "setup": "set tabstop=4 expandtab"},
    {"name": "exl_retab_single_space_kept", "lines": ["a b  c"], "keys": ":set noet<CR>:retab!<CR>", "setup": "set tabstop=4 expandtab"},
    {"name": "exl_retab_bad_arg", "lines": ["\ta"], "keys": ":retab x<CR>"},
    {"name": "exl_startinsert", "lines": ["abc"], "keys": "l:startinsert<CR>X<Esc>"},
    {"name": "exl_startinsert_bang", "lines": ["abc"], "keys": "l:startinsert!<CR>X<Esc>"},
    {"name": "exl_startreplace", "lines": ["abc"], "keys": "l:startreplace<CR>XY<Esc>"},
    {"name": "exl_startreplace_bang", "lines": ["abc"], "keys": "l:startreplace!<CR>XY<Esc>"},
    {"name": "exl_startgreplace", "lines": ["abc"], "keys": "l:startgreplace<CR>XY<Esc>"},
    {"name": "exl_startinsert_trailing", "lines": ["abc"], "keys": "l:startinsert x<CR>"},
    {"name": "exl_startinsert_after_bar", "lines": ["abc"], "keys": ":startinsert|s/a/Q/<CR>Z<Esc>"},
    {"name": "exl_stopinsert_from_ctrl_o", "lines": ["abc"], "keys": "iX<C-o>:stopinsert<CR>Y"},
    {"name": "exl_keeppatterns_search_kept", "lines": ["ab", "ab", "ab"], "keys": "/b<CR>:keeppatterns s/a/X/<CR>jn"},
    {"name": "exl_keeppatterns_no_substitute", "lines": ["ab", "ab", "ab"], "keys": "/b<CR>:keeppatterns s/a/X/<CR>j&"},
    {"name": "exl_keeppatterns_empty_pattern", "lines": ["ab", "ab", "ab"], "keys": "/b<CR>:keeppatterns s/a/X/<CR>j:s//Y/<CR>"},
    {"name": "exl_keeppatterns_address", "lines": ["ab", "ab", "ab"], "keys": "/a<CR>:keeppatterns /b/d<CR>n"},
    {"name": "exl_keepjumps_keeps_context_mark", "lines": ["a", "b", "c", "d"], "keys": "jG:keepjumps normal gg<CR>''x"},
    {"name": "exl_without_keepjumps_context_mark", "lines": ["a", "b", "c", "d"], "keys": "jG:normal gg<CR>''x"},
    {"name": "exl_keepjumps_no_change_mark", "lines": ["a", "b", "c", "d"], "keys": ":keepj norm Gx<CR>gg`.x"},
    {"name": "exl_filter_print_cursor", "lines": ["ab", "b", "cb"], "keys": ":filter /c/ %p<CR>"},
    {"name": "exl_filter_no_command", "lines": ["ab", "b", "cb"], "keys": ":filter /c/<CR>"},
    {"name": "exl_filter_other_command_runs", "lines": ["ab", "b", "cb"], "keys": ":filter /c/ 2d<CR>"},
    #     An Ex command that puts the cursor on the first non-blank sets the wanted column there (Vim's
    #     `beginline`), so a later `j` keeps it; `:>`/`:<` keep it (#1104).
    {"name": "exl_curswant_after_s", "lines": ["abc", "abc", "abc"], "keys": "ll:s/a/X/<CR>j"},
    {"name": "exl_curswant_after_g_s", "lines": ["abc", "abc", "abc"], "keys": "ll:g/a/s/b/Y/<CR>k"},
    {"name": "exl_curswant_after_sort", "lines": ["cbc", "abc", "abc"], "keys": "ll:sort<CR>j"},
    {"name": "exl_curswant_after_d", "lines": ["abc", "abc", "abc", "abc"], "keys": "ll:d<CR>j"},
    {"name": "exl_curswant_after_shift", "lines": ["abc", "abc", "abc", "abc"], "keys": "ll:>|<<CR>j"},
    {"name": "exl_curswant_after_put", "lines": ["abc", "abc", "abc", "abc"], "keys": "llyy:put<CR>j"},
    {"name": "exl_curswant_after_j", "lines": ["abc", "abc", "abc", "abc"], "keys": "ll:j<CR>j"},
    #     COMMAND-LINE COMPLETION (#613): nvim's defaults (`wildmode=full`, `wildoptions=pum`). The completed
    #     line is made observable by turning it into a substitution: `<Home>s/^/` + `<End>/<CR>` inserts it
    #     at the start of line 1 (the `<Home>` also keeps the selected match).
    {"name": "wild_command_single", "lines": ["abc"], "keys": ":wri<Tab><Home>s/^/<End>/<CR>"},
    {"name": "wild_command_after_range", "lines": ["abc"], "keys": ":%wri<Tab><Home>s/^/<End>/<CR>"},
    {"name": "wild_command_after_bar", "lines": ["abc"], "keys": ":e x | wri<Tab><Home>s/^/<End>/<CR>"},
    {"name": "wild_option_first", "lines": ["abc"], "keys": ":set nu<Tab><Home>s/^/<End>/<CR>"},
    {"name": "wild_option_next", "lines": ["abc"], "keys": ":set nu<Tab><Tab><Home>s/^/<End>/<CR>"},
    {"name": "wild_option_wraps_to_typed", "lines": ["abc"], "keys": ":set nu<Tab><Tab><Tab><Home>s/^/<End>/<CR>"},
    {"name": "wild_option_wraps_around", "lines": ["abc"], "keys": ":set nu<Tab><Tab><Tab><Tab><Home>s/^/<End>/<CR>"},
    {"name": "wild_option_s_tab_last", "lines": ["abc"], "keys": ":set nu<S-Tab><Home>s/^/<End>/<CR>"},
    {"name": "wild_option_ctrl_n_ctrl_p", "lines": ["abc"], "keys": ":set nu<Tab><C-n><C-p><C-n><Home>s/^/<End>/<CR>"},
    {"name": "wild_option_right_left", "lines": ["abc"], "keys": ":set nu<Tab><Right><Left><Right><Home>s/^/<End>/<CR>"},
    {"name": "wild_ctrl_e_restores", "lines": ["abc"], "keys": ":set nu<Tab><Tab><C-e><Home>s/^/<End>/<CR>"},
    {"name": "wild_ctrl_y_keeps", "lines": ["abc"], "keys": ":set nu<Tab><Tab><C-y><Home>s/^/<End>/<CR>"},
    {"name": "wild_typing_keeps_match", "lines": ["abc"], "keys": ":set nu<Tab>X<Home>s/^/<End>/<CR>"},
    {"name": "wild_bs_keeps_match", "lines": ["abc"], "keys": ":set nu<Tab><BS><Home>s/^/<End>/<CR>"},
    {"name": "wild_esc_abandons", "lines": ["abc"], "keys": ":s/^/X<Tab><Esc>"},
    {"name": "wild_option_short_name", "lines": ["abc"], "keys": ":set ts<Tab><Home>s/^/<End>/<CR>"},
    {"name": "wild_option_no_prefix", "lines": ["abc"], "keys": ":set nonu<Tab><Home>s/^/<End>/<CR>"},
    {"name": "wild_option_second_arg", "lines": ["abc"], "keys": ":set nu ts<Tab><Home>s/^/<End>/<CR>"},
    {"name": "wild_option_value_current", "lines": ["abc"], "keys": ":set ts=3<CR>:set ts=<Tab><Home>s/^/<End>/<CR>"},
    {"name": "wild_option_value_list", "lines": ["abc"], "keys": ":set fdm=<Tab><Tab><Home>s/^/<End>/<CR>"},
    {"name": "wild_option_value_prefix", "lines": ["abc"], "keys": ":set fdm=i<Tab><Home>s/^/<End>/<CR>"},
    {"name": "wild_ctrl_l_longest", "lines": ["abc"], "keys": ":set numb<C-l><Home>s/^/<End>/<CR>"},
    {"name": "wild_ctrl_a_all", "lines": ["abc"], "keys": ":set nu<C-a><Home>s/^/<End>/<CR>"},
    {"name": "wild_ctrl_d_keeps_line", "lines": ["abc"], "keys": ":set nonu<C-d><Home>s/^/<End>/<CR>"},
    {"name": "wild_caret_mid_word", "lines": ["abc"], "keys": ":set numberX<Left><Tab><C-y><Home>s/^/<End>/<CR>"},
    {"name": "wild_tab_is_literal_in_pattern", "lines": ["abc"], "keys": ":s/^/<Tab>/<CR>"},
    {"name": "wild_no_match_leaves_line", "lines": ["abc"], "keys": ":zzzq<Tab><Home>s/^/<End>/<CR>"},
    {"name": "wild_search_tab_inserts_nothing", "lines": ["abc", "a\tb"], "keys": "/a<Tab>b<CR>"},
    {"name": "wild_ctrl_n_is_history_when_closed", "lines": ["aa"], "keys": ":s/a/b/<CR>:<C-p><CR>"},
    #     REGISTERS (#749, #707, #816, #1022) ---------------------------------------------------------
    #     #749: a delete into a letter also fills "1; "0 "1-"9 "- take an explicit write; the motions
    #     `%` `(` `)` `{` `}` `` ` `` `/` `?` `n` `N` `*` `#` always use "1 (and "- too when unnamed).
    {"name": "rg_named_dd_fills_reg1", "lines": ["a", "b"], "keys": '"addG"1p'},
    {"name": "rg_reg0_explicit_yank", "lines": ["one", "two"], "keys": 'yyj"0yy"0p'},
    {"name": "rg_reg0_explicit_dd", "lines": ["x", "y", "z"], "keys": '"0ddj"0p"1p'},
    {"name": "rg_small_delete_explicit_dw", "lines": ["foo bar"], "keys": '"-dw"-p'},
    {"name": "rg_small_delete_explicit_dd", "lines": ["x", "y"], "keys": '"-dd"-p"1p'},
    {"name": "rg_small_delete_explicit_yw", "lines": ["x y"], "keys": '"-yw$"-p'},
    {"name": "rg_reg1_explicit_dw", "lines": ["x y z"], "keys": '"1dw$"1p'},
    {"name": "rg_reg2_explicit_dd", "lines": ["x", "y", "z"], "keys": 'dd"2ddp"2p'},
    {"name": "rg_d_percent_fills_reg1", "lines": ["(x) y"], "keys": 'd%$"1p"-p'},
    {"name": "rg_d_paragraph_one_line_fills_reg1", "lines": ["a b", "", "c"], "keys": 'wd}"1p"-p'},
    {"name": "rg_d_search_fills_reg1", "lines": ["a b c"], "keys": 'd/c<CR>"1p"-p'},
    {"name": "rg_d_sentence_fills_reg1", "lines": ["One. Two. Three."], "keys": 'd)$"1p'},
    {"name": "rg_d_backtick_mark_fills_reg1", "lines": ["abc def"], "keys": 'wmb0d`b$"1p'},
    {"name": "rg_d_star_fills_reg1", "lines": ["ab cd ab"], "keys": 'd*$"1p'},
    {"name": "rg_named_d_percent_fills_reg1", "lines": ["(x) y"], "keys": '"ad%$"1p"ap'},
    {"name": "rg_dw_spares_reg1", "lines": ["one", "a b"], "keys": 'ddwdw"1p'},
    {"name": "rg_named_dd_then_qaq_keeps_unnamed", "lines": ["a", "b"], "keys": '"addqaqp'},
    {"name": "rg_append_dd_unnamed", "lines": ["a", "b", "c"], "keys": '"ayyj"Addp'},
    #     #707: Visual `"ap` deletes into the unnamed register, never into "a.
    {"name": "rg_visual_named_put_keeps_named", "lines": ["foo bar"], "keys": '"ayiwwviw"apb"aP'},
    {"name": "rg_visual_named_put_unnamed_gets_deleted", "lines": ["foo bar"], "keys": '"ayiwwviw"ap0P'},
    {"name": "rg_visual_line_named_put", "lines": ["foo", "bar"], "keys": '"ayyjV"apgg"ap"1p'},
    {"name": "rg_visual_reg0_put", "lines": ["foo bar"], "keys": 'yiwwviw"0p0"0P'},
    {"name": "rg_visual_numbered_put", "lines": ["a", "b", "c"], "keys": 'ddjV"1pgg"1P'},
    #     #816: `.` after a put from "1-"8 steps to the next numbered register; "9 stays.
    {"name": "rg_dot_walks_numbered", "lines": ["a", "b", "c"], "keys": 'ddddp"1p.'},
    {"name": "rg_dot_walks_numbered_undo", "lines": ["a", "b", "c", "d"], "keys": 'dddddd"1pu.u.'},
    {"name": "rg_dot_walks_numbered_twice", "lines": ["a", "b", "c", "d"], "keys": 'dddddd"1p..'},
    {"name": "rg_dot_numbered_delete_steps", "lines": ["a", "b", "c", "d", "e"], "keys": '"1dd.G"1p"2p'},
    {"name": "rg_dot_reg9_stays", "lines": ["a", "b", "c", "d"], "keys": 'dddd"9p.'},
    {"name": "rg_dot_named_put_stays", "lines": ["a", "b"], "keys": '"ayy"ap.'},
    #     #1022: a write to a read-only register fails, and an unknown name is no register.
    {"name": "rg_readonly_slash_dd_fails", "lines": ["a", "b"], "keys": 'yy"/ddp'},
    {"name": "rg_readonly_colon_dd_fails", "lines": ["a", "b"], "keys": 'yy":ddp'},
    {"name": "rg_readonly_dot_yy_fails", "lines": ["a", "b"], "keys": 'yyj".yyp'},
    {"name": "rg_readonly_percent_dw_fails", "lines": ["ab cd"], "keys": 'yl"%dwp'},
    {"name": "rg_readonly_slash_x_fails", "lines": ["abc"], "keys": '"/xp'},
    {"name": "rg_readonly_slash_db_fails", "lines": ["ab cd"], "keys": '$"/dbx'},
    {"name": "rg_readonly_slash_cw", "lines": ["ab cd"], "keys": '"/cwX<Esc>'},
    {"name": "rg_readonly_slash_visual_d", "lines": ["ab cd"], "keys": 'wvl"/dx'},
    {"name": "rg_tilde_put_empty", "lines": ["a", "b"], "keys": 'yy"~p'},
    {"name": "rg_tilde_dd_fails", "lines": ["a", "b"], "keys": 'yy"~ddp'},
    {"name": "rg_unknown_bang_dd", "lines": ["a", "b"], "keys": 'yy"!ddp'},
    {"name": "rg_unknown_question_p", "lines": ["a", "b"], "keys": 'yy"?p'},
    {"name": "rg_hash_put_no_alternate", "lines": ["a", "b"], "keys": 'yy"#p'},
    {"name": "rg_hash_yy_writes_reg0", "lines": ["a", "b"], "keys": 'j"#yyk"0p'},
    {"name": "rg_hash_dd", "lines": ["a", "b", "c"], "keys": '"#dd"0p"1p'},
    {"name": "rg_expr_newline_is_linewise", "lines": ["a", "b"], "keys": '"="x\\n"<CR>p'},
    {"name": "rg_expr_plain_is_charwise", "lines": ["a", "b"], "keys": '"="x"<CR>p'},
    #     SEARCH WITH ANCHORS AND ZERO-WIDTH MATCHES (#737, #705, #1046-#1049): what `/`, `?`, `n`, `N` and
    #     `:s///g` do with `^`, `$`, `\<`, `\zs`, `\ze` and overlapping matches.
    {"name": "srx_caret_next_line", "lines": ["x", "foo"], "keys": "/^foo<CR>"},
    {"name": "srx_dollar_mid_buffer", "lines": ["foo x", "a foo"], "keys": "/foo$<CR>"},
    {"name": "srx_caret_dollar_empty_line", "lines": ["a", "", "b"], "keys": "/^$<CR>iZ<Esc>"},
    {"name": "srx_bare_dollar", "lines": ["ab", "cd"], "keys": "/$<CR>"},
    {"name": "srx_bare_dollar_n", "lines": ["ab", "cd", "ef"], "keys": "/$<CR>n"},
    {"name": "srx_bare_caret", "lines": ["ab", "cd"], "keys": "l/^<CR>"},
    {"name": "srx_back_caret", "lines": ["ab", "cd"], "keys": "G$?^<CR>"},
    {"name": "srx_back_dollar", "lines": ["ab", "cd"], "keys": "G$?$<CR>"},
    {"name": "srx_back_caret_dollar", "lines": ["a", "", "b"], "keys": "G?^$<CR>iZ<Esc>"},
    {"name": "srx_zs_at_end", "lines": ["xx foo bar"], "keys": "/foo\\zs<CR>"},
    {"name": "srx_bow_bare", "lines": ["xx foo bar"], "keys": "/\\<<CR>"},
    {"name": "srx_bow_n", "lines": ["xx foo bar"], "keys": "/\\<<CR>n"},
    {"name": "srx_bow_N", "lines": ["xx foo bar"], "keys": "$/\\<<CR>N"},
    {"name": "srx_zs_from_start", "lines": ["foobar foobar"], "keys": "/foo\\zsbar<CR>"},
    {"name": "srx_zs_from_start_n", "lines": ["foobar foobar"], "keys": "/foo\\zsbar<CR>n"},
    {"name": "srx_zs_back", "lines": ["foobar foobar"], "keys": "$?foo\\zsbar<CR>"},
    {"name": "srx_back_overlap", "lines": ["aaa"], "keys": "$?aa<CR>"},
    {"name": "srx_N_overlap", "lines": ["aaa"], "keys": "$/aa<CR>N"},
    {"name": "srx_fwd_overlap", "lines": ["aaaa"], "keys": "/aa<CR>"},
    {"name": "srx_fwd_overlap_n", "lines": ["aaaa"], "keys": "/aa<CR>n"},
    {"name": "srx_s_ze_resumes_at_ze", "lines": ["ababab"], "keys": ":s/\\v(ab)\\zeab/-/g<CR>"},
    {"name": "srx_s_eow_g", "lines": ["a b"], "keys": ":s/\\>/-/g<CR>"},
    {"name": "srx_s_eow_g_words", "lines": ["foo bar"], "keys": ":s/\\>/-/g<CR>"},
    {"name": "srx_s_star_g", "lines": ["abc"], "keys": ":s/x*/-/g<CR>"},
    {"name": "srx_s_alt_eol_g", "lines": ["ab"], "keys": ":s/a\\|$/-/g<CR>"},
    {"name": "srx_s_count_brace", "lines": ["caaa"], "keys": ":s/a\\{2}/x/<CR>"},
    {"name": "srx_s_ws_no_newline", "lines": ["x a", "b"], "keys": "/a\\s<CR>"},
    {"name": "srx_s_ws_gnd", "lines": ["a", "b"], "keys": "/\\s<CR>gnd"},
    {"name": "srx_s_zs_g", "lines": ["foobar foobar"], "keys": ":s/foo\\zsbar/X/g<CR>"},
    {"name": "srx_d_search_zero_width", "lines": ["xx foo bar"], "keys": "d/\\<f<CR>"},
    {"name": "srx_count_search_bow", "lines": ["aa bb cc dd"], "keys": "2/\\<<CR>"},
    {"name": "srx_move_to_dollar", "lines": ["ab cd", "ef"], "keys": "l/$<CR>"},
    {"name": "srx_d_to_dollar", "lines": ["ab cd", "ef"], "keys": "ld/$<CR>"},
    {"name": "srx_v_to_dollar", "lines": ["ab cd", "ef"], "keys": "lv/$<CR>d"},
    # Visual `/` and `?` open the search line and extend the selection to the match (#1113).
    {"name": "srx_v_slash_word", "lines": ["ab cd ef"], "keys": "v/cd<CR>d"},
    {"name": "srx_v_question_back", "lines": ["ab cd ef"], "keys": "$v?cd<CR>d"},
    {"name": "srx_v_slash_then_n", "lines": ["a x b x c x"], "keys": "v/x<CR>nd"},
    {"name": "srx_v_slash_offset_e", "lines": ["ab cdef gh"], "keys": "v/cd/e<CR>y$p"},
    {"name": "srx_V_slash_lines", "lines": ["a", "b", "xc", "d"], "keys": "V/x<CR>d"},
    {"name": "srx_ctrl_v_slash_block", "lines": ["abcd", "efgh", "ijkl"], "keys": "<C-v>/k<CR>d"},
    {"name": "srx_v_slash_not_found", "lines": ["ab cd"], "keys": "lv/zz<CR>d"},
    {"name": "srx_v_slash_esc_keeps_selection", "lines": ["ab cd"], "keys": "lvl/cd<Esc>d"},
    {"name": "srx_v_slash_count", "lines": ["a x b x c x"], "keys": "v2/x<CR>d"},
    {"name": "srx_x_after_dollar", "lines": ["ab", "cd"], "keys": "/$<CR>x"},
]


def generate(path: Path) -> int:
    """Capture every fixture's `expect` from the oracle and write the corpus as JSON-in-YAML."""
    pin = read_pin()
    version_line = nvim_version()
    assert_pin(version_line, pin)

    names = [spec["name"] for spec in FIXTURES]
    dupes = sorted({n for n in names if names.count(n) > 1})
    if dupes:
        # The ratchet (known-divergences.txt) names fixtures, so a name must pick out one fixture.
        raise OracleError(f"duplicate fixture names: {', '.join(dupes)}")

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
                    "emsg": state["emsg"],
                    "failed": state["failed"],
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
            "captured_observables": ["text", "cursor", "reg_unnamed", "reg0", "emsg", "failed"],
            "ruse_compare_observables": ["text", "cursor", "reg_unnamed", "reg0", "emsg"],
            "feed": "nvim_feedkeys(keys, 'xt'): as typed, so keys after a failed command still run",
            "error_note": (
                "emsg is the E-number of the last error message the keys raised (v:errmsg), or ''. "
                "failed is whether any command failed (an error, or a Normal-mode beep), observed as "
                "Vim flushing the keys queued after it when they are fed not as typed. Best effort: "
                "a failure inside a macro replay is not seen. failed is captured but not compared: "
                "ruse has no bell (#901)."
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
        # #901: the keys are typed, so a failed `fz` does not swallow the `x` after it; the failure is
        # still observed (`failed`), and an error carries its E-number (`emsg`).
        (
            "typed keys survive a failed motion",
            ["abc"],
            "lfzx",
            lambda s: s["text"] == ["ac"] and s["failed"] and s["emsg"] == "",
        ),
        (
            "a missing pattern is E486",
            ["abc"],
            "/zzz<CR>",
            lambda s: s["emsg"] == "E486" and s["failed"],
        ),
        (
            "a plain edit neither fails nor errs",
            ["abc"],
            "x",
            lambda s: not s["failed"] and not s["emsg"],
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
