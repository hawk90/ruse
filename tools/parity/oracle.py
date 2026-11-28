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
       fed with `nvim_feedkeys(nvim_replace_termcodes(keys, true, true, true), 'xt', false)` — mode
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

Fixture inputs live in tools/parity/fixtures/nvim/*.jsonl, not here (#1590). Add new fixtures to
`i<issue>.jsonl` for their issue — never append to the `NN-legacy.jsonl` files; see FIXTURE_DIR below.

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
-- A `bytes` fixture (#1232) passes its lines, and reads back its text and registers, as hex: JSON cannot
-- carry a byte that is not UTF-8.
local function unhex(s) return (s:gsub('..', function(h) return string.char(tonumber(h, 16)) end)) end
local function hex(s) return (s:gsub('.', function(c) return string.format('%%02x', c:byte()) end)) end
local function out(s) if input.hex then return hex(s) end return s end
if input.hex then
  for i, l in ipairs(input.lines) do input.lines[i] = unhex(l) end
end
vim.api.nvim_buf_set_lines(0, 0, -1, false, input.lines)
vim.api.nvim_win_set_cursor(0, {1, 0})
-- A fixture about a file type (#1142) names the buffer's file: its type and indent script then apply, as
-- for an opened file. Nothing is read or written.
if input.file ~= nil and input.file ~= '' then
  vim.cmd('silent file ' .. vim.fn.fnameescape(input.file))
  vim.cmd('filetype indent on')
  vim.cmd('filetype detect')
end
if input.setup ~= nil and input.setup ~= '' then
  vim.cmd(input.setup)
end
vim.v.errmsg = ''
-- Where the undo walk (#900) stops: the state the keys started from.
local base_seq = vim.fn.undotree().seq_cur
-- `nvim_buf_set_lines` sets `'.`; with no change since, that mark is the oracle's, not the keys'.
local base_tick = vim.b.changedtick
local tc = vim.api.nvim_replace_termcodes(input.keys, true, true, true)
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
-- #900: a mark as {line, byte col}, or nil when unset.
local function pos(m)
  local p = vim.fn.getpos(m)
  if p[2] == 0 then return nil end
  return { p[2], p[3] - 1 }
end
local mode = vim.api.nvim_get_mode().mode
local marks = vim.empty_dict()
for m in ("abcdefghijklmnopqrstuvwxyz<>[].^"):gmatch(".") do
  marks[m] = pos("'" .. m)
end
if vim.b.changedtick == base_tick then marks['.'] = nil end
local regs = vim.empty_dict()
for r in ("123456789-"):gmatch(".") do
  regs[r] = { out(vim.fn.getreg(r)), vim.fn.getregtype(r) }
end
-- The Visual area being made: its mode and the end the cursor is not on (`getpos('v')`).
local visual = vim.NIL
if mode == 'v' or mode == 'V' or mode == '\22' then
  visual = { mode, pos('v') }
end
local state = {
  text        = vim.tbl_map(out, vim.api.nvim_buf_get_lines(0, 0, -1, false)),
  cursor      = vim.api.nvim_win_get_cursor(0),
  reg_unnamed = { out(vim.fn.getreg('"')), vim.fn.getregtype('"') },
  reg0        = { out(vim.fn.getreg('0')), vim.fn.getregtype('0') },
  mode        = vim.api.nvim_get_mode().mode,
  emsg        = vim.v.errmsg:match('^(E%%d+)') or '',
  -- Per line, nvim's fold state (#903): the closed fold holding it (`foldclosed`/`foldclosedend`, -1 for
  -- none) and its fold level. Read only; recorded for the fixtures marked `folds`.
  folds       = (function()
    local out = {}
    for l = 1, vim.api.nvim_buf_line_count(0) do
      out[l] = { vim.fn.foldclosed(l), vim.fn.foldclosedend(l), vim.fn.foldlevel(l) }
    end
    return out
  end)(),
  -- The first line in the window (`line('w0')`): recorded for the fixtures marked `topline` (#1141).
  topline     = vim.fn.line('w0'),
  -- #900: what the fixtures that observe them record (see `_observes`).
  marks       = marks,
  vmode       = vim.fn.visualmode(),
  regs        = regs,
  visual      = visual,
}
-- The undo walk changes the buffer, so it comes last: the text after each `:undo` back to where the keys
-- started (at most 20 steps), for the fixtures that observe undo.
if input.undo then
  local undo = {}
  for _ = 1, 20 do
    local before = vim.fn.undotree().seq_cur
    if before <= base_seq then break end
    vim.cmd('silent! undo')
    if vim.fn.undotree().seq_cur == before then break end
    table.insert(undo, vim.tbl_map(out, vim.api.nvim_buf_get_lines(0, 0, -1, false)))
  end
  state.undo = undo
end
io.write(vim.json.encode(state))
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


def _latin1_lines(lines: list[str]) -> list[str]:
    """A `bytes` fixture's lines as hex for the probe: each char of a line is one byte (Latin-1), so a line
    can hold a byte that is not UTF-8 (#1232). `_from_latin1_hex` reads the probe's answer back."""
    return [line.encode("latin-1").hex() for line in lines]


def _from_latin1_hex(s: str) -> str:
    """The probe's hex answer as a Latin-1 string: one char per byte, the spelling `bytes` fixtures use."""
    return bytes.fromhex(s).decode("latin-1")


# The marks and registers the probe reads (#900), in the order the corpus records them.
MARK_NAMES = "abcdefghijklmnopqrstuvwxyz<>[].^"
REG_NAMES = "123456789-"
# A mark's column nvim leaves at MAXCOL: the end of a linewise area (`'>` after `V`, `']` after `yj`).
MAXCOL = 2147483647


def _mark(p: list[int] | None) -> list[int] | None:
    """A probe mark `[line, col0]` with MAXCOL as -1: a linewise end, whose column Vim leaves unset."""
    if p is None:
        return None
    return [p[0], -1 if p[1] >= MAXCOL - 1 else p[1]]


def run_neovim(
    lines: list[str],
    keys: str,
    setup: str = "",
    file: str = "",
    raw: bool = False,
    undo: bool = False,
) -> dict:
    """Run the pinned Neovim on `lines`, feed `keys` in Normal mode, and return the settled state.

    Returns {text, cursor:[row1,col0], reg_unnamed:{text,type}, reg0:{text,type}, mode, emsg, failed,
    nvim_version}. The read happens strictly after synchronous feedkeys — see the module docstring for
    why that is the whole point. `setup` is an optional ex command (e.g. `set shiftwidth=4 expandtab`)
    applied to the fresh process before the keys, so a CONFIG-DEPENDENT op can be observed under a
    config that matches ruse's defaults instead of nvim's factory defaults (see the shift_right_line
    fixture). `file` optionally names the buffer's file (`fixture.py`), so its file type and indent
    script apply (`filetype indent on`); the comparator opens ruse's buffer under the same name (#1142).

    The keys are fed AS TYPED, so the keys after a failed command still run, as they do for a user.
    Two error observables (#901) record WHETHER something failed, never the message text: `emsg` is the
    E-number of the last error message the keys raised (`v:errmsg`, cleared first), or ""; `failed` is
    whether any command failed, an error or a Normal-mode beep, observed in a second fresh process as
    Vim flushing the keys queued after the failure (see the probe).

    #900 adds `mode` (`nvim_get_mode()`), `marks` (a-z, `<`, `>`, `[`, `]`, `.`, `^` as `[line, col0]`,
    the set ones only, col -1 for MAXCOL), `vmode` (`visualmode()`), `regs` (`"1`-`"9` and `"-`), `visual`
    (in Visual mode, `[mode, [line, col0]]` of the end the cursor is not on) and, when `undo` is asked
    for, `undo`: the text after each `:undo` back to where the keys started.
    """
    payload = {"lines": lines, "keys": keys, "setup": setup, "file": file, "undo": undo}
    if raw:
        # `raw`: the lines are Latin-1 spellings of bytes (a `bytes` fixture, #1232); they travel as hex.
        payload = {**payload, "lines": _latin1_lines(lines), "hex": True}
    got = _nvim(payload)
    failed = _nvim({**payload, "probe_failed": True})["failed"]
    ru_text, ru_type = got["reg_unnamed"]
    r0_text, r0_type = got["reg0"]
    text = got["text"]
    unhex = _from_latin1_hex if raw else (lambda t: t)
    if raw:
        text = [_from_latin1_hex(t) for t in text]
        ru_text, r0_text = _from_latin1_hex(ru_text), _from_latin1_hex(r0_text)
    # In a fixed order: the probe's JSON objects come out in Lua's hash order, which differs between
    # machines, so the corpus would not regenerate byte-identically (#904).
    regs = {
        name: {"text": unhex(got["regs"][name][0]), "type": _regtype(got["regs"][name][1])}
        for name in REG_NAMES
    }
    marks = {name: _mark(got["marks"][name]) for name in MARK_NAMES if got["marks"].get(name)}
    visual = got["visual"]
    if visual is not None:
        visual = [_regtype(visual[0]), _mark(visual[1])]
    return {
        "text": text,
        "cursor": got["cursor"],
        "reg_unnamed": {"text": ru_text, "type": _regtype(ru_type)},
        "reg0": {"text": r0_text, "type": _regtype(r0_type)},
        "mode": got["mode"],
        "emsg": got["emsg"],
        "failed": failed,
        "folds": got["folds"],
        "topline": got["topline"],
        "marks": marks,
        "vmode": _regtype(got["vmode"]),
        "regs": regs,
        "visual": visual,
        "undo": [[unhex(t) for t in u] for u in got.get("undo", [])] if undo else None,
        "nvim_version": nvim_version(),
    }


# --- The fixture corpus: ALREADY-IMPLEMENTED ruse ops. `expect` is captured from the oracle, never
#     hand-written; the (lines, keys) of each fixture are the only human-authored part. `<Esc>` is passed
#     verbatim to nvim_replace_termcodes and is understood identically by the Rust key tokenizer;
#     `<lt>` is a literal `<` on both sides (do_lt, #764), so keys can type the text `<Esc>`.
#
# WHERE FIXTURES LIVE (#1590): not in this script but in FIXTURE_DIR, as JSON Lines. Each line is blank, a
# `#` comment, or one fixture as a JSON object on one line (`name`, `lines`, `keys`, and optionally `setup`,
# `file`, `observe`, `folds`, `topline`, `bytes`). JSON strings carry tabs, ESC, CTRL chars, trailing spaces
# and the Latin-1 chars of a `bytes` fixture exactly; write control and invisible chars as `\uXXXX` escapes.
#
# THE APPEND RULE: add new fixtures to `i<issue>.jsonl` (e.g. `i1590.jsonl`) for the issue they belong to,
# creating it if needed, so parallel PRs do not append to the same file. The `NN-legacy.jsonl` files hold the
# fixtures that predate the split, in their original order; edit them in place, but do not append to them.
# Load order: the `NN-*.jsonl` files by name, then the `i<issue>.jsonl` files by issue number; fixtures keep
# their order within a file. Any other file name in FIXTURE_DIR is an error.
#
# Recurring values the fixtures spell out literally:
#   setup "set laststatus=0 scrolloff=3" — gives nvim a 23-row window (an 80x24 terminal with no status line
#     under the text), which the ruse side replays at 'cmdheight' 0 (tests/parity_compare.rs, RFC-0027), and
#     ruse's scrolloff; used with a 100-line buffer "line 1" .. "line 100" for the page-scroll fixtures.
#   setup "call append('$', '') | $d _ | call cursor(1, 1)" — `nvim_buf_set_lines` saves the buffer's first
#     line for `U`; appending a line and deleting it again as a whole line drops that, as `dd` does, so a
#     `U` fixture starts with nothing to restore.
#   setup UNDO_SETUP — an undo break after the oracle's `nvim_buf_set_lines`, so `u` undoes only the
#     fixture's change (and the fixture records `undo`, see _observes).
#   setup "set shiftwidth=4 expandtab" — a shift under ruse's default indent (4 columns, spaces).
#     'nostartofline' stays nvim's default: ruse follows it (#1176).
UNDO_SETUP = "let &undolevels = &undolevels"
FIXTURE_DIR = Path(__file__).resolve().parent / "fixtures" / "nvim"
_LEGACY_FILE = re.compile(r"\d\d-[a-z0-9-]+\.jsonl")
_ISSUE_FILE = re.compile(r"i([1-9][0-9]*)\.jsonl")


def fixture_files(directory: Path = FIXTURE_DIR) -> list[Path]:
    """The fixture files in load order: `NN-*.jsonl` by name, then `i<issue>.jsonl` by issue number."""
    legacy: list[Path] = []
    issues: list[tuple[int, Path]] = []
    for path in directory.iterdir():
        if _LEGACY_FILE.fullmatch(path.name):
            legacy.append(path)
        elif m := _ISSUE_FILE.fullmatch(path.name):
            issues.append((int(m.group(1)), path))
        else:
            raise OracleError(f"{path}: not a fixture file name (NN-<name>.jsonl or i<issue>.jsonl)")
    return sorted(legacy) + [p for _, p in sorted(issues)]


def load_fixtures(directory: Path = FIXTURE_DIR) -> list[dict]:
    """Every fixture in FIXTURE_DIR, in load order. A malformed line names its file and line."""
    fixtures: list[dict] = []
    for path in fixture_files(directory):
        # Split on "\n" only: str.splitlines would also break at U+0085, U+2028 and other chars a line may hold.
        for n, line in enumerate(path.read_text(encoding="utf-8").split("\n"), 1):
            if not line.strip() or line.lstrip().startswith("#"):
                continue
            try:
                spec = json.loads(line)
            except json.JSONDecodeError as exc:
                raise OracleError(f"{path}:{n}: {exc}") from exc
            if not isinstance(spec, dict) or not all(k in spec for k in ("name", "lines", "keys")):
                raise OracleError(f"{path}:{n}: a fixture is an object with name, lines and keys")
            fixtures.append(spec)
    return fixtures


FIXTURES: list[dict] = load_fixtures()


# #900: the observables beyond text, cursor, the two registers and the error that a fixture records. A
# fixture asks with `observe`; whole groups ask by name prefix, so the existing fixtures about marks,
# registers and Visual mode check them without each spelling it out. `undo` needs UNDO_SETUP's undo
# break, so the walk back stops where the fixture's keys started.
OBSERVE_GROUPS: dict[str, tuple[str, ...]] = {
    "marks": ("mk_", "sc_mark_", "v_", "visual_", "vb_"),
    "regs": ("rg_", "d_", "sc_reg"),
    "visual": ("v_", "visual_", "vb_", "blk_", "block_"),
}


def _observes(spec: dict) -> set[str]:
    """The #900 observables `spec` records: its own `observe` list, its groups', and `undo` under
    UNDO_SETUP."""
    got = set(spec.get("observe", ()))
    for what, prefixes in OBSERVE_GROUPS.items():
        if spec["name"].startswith(prefixes):
            got.add(what)
    if UNDO_SETUP in spec.get("setup", ""):
        got.add("undo")
    return got


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
        file = spec.get("file", "")
        raw = bool(spec.get("bytes"))
        observe = _observes(spec)
        state = run_neovim(spec["lines"], spec["keys"], setup, file, raw, "undo" in observe)
        entry = {
            "name": spec["name"],
            "lines": spec["lines"],
            "keys": spec["keys"],
        }
        # `bytes`: the lines, text and register texts spell bytes in Latin-1, one char per byte (#1232).
        if raw:
            entry["bytes"] = True
        # Emit `setup` only for the fixtures that carry one, so the corpus records exactly which runs were
        # config-matched (and stays byte-identical for the config-independent majority).
        if setup:
            entry["setup"] = setup
        # Likewise `file`: the buffer's file name, which picks its type (and indent) on both sides.
        if file:
            entry["file"] = file
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
                    # The mode the keys end in, for every fixture (#900).
                    "mode": state["mode"],
                    # Only the fixtures about folds record them, so the rest stay byte-identical.
                    **({"folds": state["folds"]} if spec.get("folds") else {}),
                    # And only those about the view record its top line (#1141).
                    **({"topline": state["topline"]} if spec.get("topline") else {}),
                    # And the #900 observables only for the fixtures that observe them (`_observes`).
                    **(
                        {"marks": state["marks"], "vmode": state["vmode"]}
                        if "marks" in observe
                        else {}
                    ),
                    **({"regs": state["regs"]} if "regs" in observe else {}),
                    **({"visual": state["visual"]} if "visual" in observe else {}),
                    **({"undo": state["undo"]} if "undo" in observe else {}),
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
            "captured_observables": [
                "text", "cursor", "reg_unnamed", "reg0", "emsg", "failed", "mode", "folds", "topline",
                "marks", "vmode", "regs", "visual", "undo",
            ],
            "ruse_compare_observables": [
                "text", "cursor", "reg_unnamed", "reg0", "emsg", "mode", "folds", "topline",
                "marks", "vmode", "regs", "visual", "undo",
            ],
            "mode_note": "mode is nvim_get_mode().mode after the keys, recorded for every fixture.",
            "observe_note": (
                "marks (with vmode), regs, visual and undo are recorded only for the fixtures that observe "
                "them (a fixture's `observe`, or its group: see _observes in oracle.py). marks: a-z, <, >, "
                "[, ], ., ^ as [line, col0], the set ones only; col -1 is MAXCOL, the end of a linewise area, "
                "whose column is not compared. vmode: visualmode(). regs: \"1-\"9 and \"-. visual: in Visual "
                "mode, [mode, [line, col0]] of the end the cursor is not on, else null. undo: the text after "
                "each :undo back to where the keys started (at most 20), for fixtures run under UNDO_SETUP."
            ),
            "topline_note": "topline is recorded only for fixtures about the view: line('w0').",
            "folds_note": (
                "folds is recorded only for fixtures about folds: per line, "
                "[foldclosed(l), foldclosedend(l), foldlevel(l)]."
            ),
            "bytes_note": (
                "a fixture with bytes: true spells its lines, text and register texts in Latin-1, one "
                "char per byte, so a line can hold bytes that are not UTF-8."
            ),
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
    # The corpus is compared as bytes, so the serialized state must not change either: key order included
    # (two dicts with the same items compare equal in any order, #904).
    if json.dumps(a) != json.dumps(b):
        _fail("determinism: the serialized state differs across identical runs")
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

    # 4. The #900 observables, each against an answer known without this harness.
    observed = [
        ("dd fills \"1 linewise", ["a", "b"], "dd", False,
         lambda s: s["regs"]["1"] == {"text": "a\n", "type": "linewise"}),
        ("dw fills \"- charwise", ["foo bar"], "dw", False,
         lambda s: s["regs"]["-"] == {"text": "foo ", "type": "charwise"}),
        ("ma sets 'a where the cursor is", ["abc", "def"], "jlma", False,
         lambda s: s["marks"].get("a") == [2, 1]),
        ("no change leaves '. unset (not the oracle's set_lines)", ["abc"], "l", False,
         lambda s: "." not in s["marks"]),
        ("Vj is linewise Visual, anchored on line 1", ["a", "b"], "Vj", False,
         lambda s: s["mode"] == "V" and s["visual"] == ["linewise", [1, 0]]),
        ("V<Esc> leaves Normal mode and visualmode() V", ["a"], "V<Esc>", False,
         lambda s: s["mode"] == "n" and s["visual"] is None and s["vmode"] == "linewise"),
        ("two undo steps walk back through two texts", ["abc"], "xx", True,
         lambda s: s["undo"] == [["bc"], ["abc"]]),
    ]
    for label, lines, keys, undo, ok in observed:
        state = run_neovim(lines, keys, UNDO_SETUP if undo else "", undo=undo)
        if not ok(state):
            _fail(f"observable disagreement: {label} — got {state}")
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
