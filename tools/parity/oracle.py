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
    {"name": "dG_to_last_line", "lines": ["alpha", "beta", "gamma"], "keys": "dG"},
    {"name": "dj_linewise", "lines": ["alpha", "beta", "gamma"], "keys": "dj"},
    {"name": "c_to_eol", "lines": ["hello world"], "keys": "c$END<Esc>"},
    {"name": "y_eol_then_paste", "lines": ["hello"], "keys": "y$p"},
    {"name": "count_dd", "lines": ["alpha", "beta", "gamma"], "keys": "2dd"},
    {"name": "count_x", "lines": ["abcdef"], "keys": "3x"},
    {"name": "d_paragraph_fwd", "lines": ["one", "two", "", "three"], "keys": "d}"},
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
    #     operators on empty / single-char / last line ------------------------------------------------
    {"name": "x_on_empty_line", "lines": [""], "keys": "x"},
    {"name": "dw_on_empty_line", "lines": [""], "keys": "dw"},
    {"name": "dd_on_last_line", "lines": ["alpha", "beta"], "keys": "Gdd"},
    {"name": "j_past_last_line", "lines": ["alpha", "beta"], "keys": "Gj"},
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
    # PROBE — blockwise over a SHORT interior line. Vim preserves the desired column (curswant) across the
    # short line so the block stays full-width; ruse recomputes the cursor column from each line's actual
    # length, so navigating down THROUGH a short line collapses the block's right edge. Same curswant family
    # as the i_CTRL-O append-column gap — a named follow-up.
    {"name": "block_ragged_short_line", "lines": ["abcd", "x", "efgh"], "keys": "l<C-v>lljjd"},
    #     REPLACE MODE (R): overwrite policy + <BS> restores the overwritten char / deletes an appended one.
    {"name": "replace_basic_overwrite", "lines": ["hello"], "keys": "Rxy<Esc>"},
    {"name": "replace_past_eol_appends", "lines": ["ab"], "keys": "RXYZ<Esc>"},
    {"name": "replace_backspace_restores", "lines": ["hello"], "keys": "Rxy<BS><BS><Esc>"},
    {"name": "replace_backspace_deletes_appended", "lines": ["ab"], "keys": "RXYZ<BS><Esc>"},
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
    # PROBES — i_CTRL-O's EOL APPEND-COLUMN (curswant) preservation: after `$` (or an `A` whose append
    # column survives a `dd`), Vim keeps the return-to-insert cursor at/past end-of-line so the next char
    # appends. ruse computes a pure Normal-mode motion cursor and lands ON the last char, so it inserts one
    # position early (`h<X>i` not `hi<X>`; `<X>beta` not `beta<X>`). Documented divergence — curswant/append-
    # column modeling is a named follow-up; F-024 stays active until it (and `gR`) land.
    {"name": "ctrl_o_dollar_append", "lines": ["hi"], "keys": "i<C-o>$X<Esc>"},
    {"name": "ctrl_o_dd_delete_line", "lines": ["alpha", "beta"], "keys": "A<C-o>ddX<Esc>"},
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
