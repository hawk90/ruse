#!/usr/bin/env python3
"""Emacs command-semantics oracle — the executable half of the Emacs parity axis.

This is the Emacs sibling of tools/parity/oracle.py (the Neovim oracle). It runs the pinned Emacs as a
black box, applies a fixture's editing COMMANDS to a scratch buffer, and reads back the resulting state.
The captured state becomes the `expect` of a fixture; a fixture is a hand-verified claim about what
Emacs's editing commands *do*, pinned to an exact upstream revision (spec/parity/upstreams.yaml).

WHY COMMANDS, NOT KEYS (the trap the Neovim oracle documents and this harness sidesteps):
    Two earlier Emacs oracle attempts drove KEYS and both corrupted their observation — `emacs --batch
    execute-kbd-macro` left the buffer empty while the kill-ring was correct, and `emacs --batch
    read-from-minibuffer` hung forever (see tools/parity/oracle.py, module docstring). The failures were
    KEY replay. What ruse's Emacs profile actually needs to conform to is COMMAND SEMANTICS — what
    `kill-region` / `forward-word` / `exchange-point-and-mark` DO — so this harness drives commands
    directly with `call-interactively`, exactly as a keypress dispatches through Emacs's command loop
    (it supplies each command's `(interactive)` arguments), with no minibuffer and no macro engine in the
    path. A fixture's `ops` are Emacs command NAMES; the same names resolve on the ruse side through the
    M-x registry (apps/tui `emacs_command_by_name`), so fixture -> oracle -> ruse share one vocabulary.

THE NON-CORRUPTION TECHNIQUE (this harness's answer to the oracle hazard):
    1. Observation is READ-ONLY and happens AFTER mutation, never through a mutating call. Commands are
       applied with `call-interactively`; every read (`buffer-string`, `point`, `mark`, `car kill-ring`)
       runs against already-settled state. No read is itself an editing command (the `vim -es` /
       `execute-kbd-macro` trap).
    2. One fresh `emacs --batch` PROCESS per fixture. `kill-ring` (and `mark-ring`, registers, ...) are
       GLOBAL variables that persist across `with-temp-buffer` calls in one image — the exact analogue of
       Neovim's shada leak. A fresh process per fixture is the only leak-proof isolation, so every fixture
       pays for its own `emacs -Q --batch`.
    3. The command loop's `this-command`/`last-command` are threaded by hand. In batch,
       `call-interactively` does NOT set `this-command` (in real Emacs the command loop sets it before
       dispatch); only commands that assign it internally do — e.g. every kill routes through
       `kill-region`, which sets `this-command` to `kill-region`. So the probe, before each call, promotes
       the prior `this-command` to `last-command` AND sets `this-command` to the command about to run.
       This makes KILL ACCUMULATION faithful in BOTH directions: consecutive kills append to one kill-ring
       entry, and a non-kill command between kills (leaving the stale `kill-region` behind otherwise)
       correctly BREAKS the run so the next kill starts a fresh entry — exactly as the equivalent
       interactive keypress sequence would.
    3. The pin is VERIFIED, not assumed. `emacs --version` is parsed and asserted to carry the pinned
       version number (spec/parity/upstreams.yaml emacs.version_label), and that version string is
       recorded in every emitted document. A wrong binary refuses rather than silently recording a lie.

SCOPE OF THE SEED CORPUS (char vs byte): point/mark are emitted as 0-based CHARACTER offsets. ruse's
core addresses the buffer by BYTE offset; for the ASCII seed corpus the two coincide, so the ruse
comparator can compare them directly. Multibyte fixtures (where char != byte) are a later expansion,
exactly as the Neovim corpus added its unicode fixtures after the ASCII core — see the corpus `note`.

Stdlib only (D-034): no PyYAML, no pip deps. The fixture corpus is emitted as JSON, which is a strict
subset of YAML 1.2 — so `corpus.yaml` is valid YAML *and* parses with `serde_json` on the Rust side.

Usage:
    python3 tools/parity/emacs_oracle.py --selftest        # prove non-corruption + determinism; gates the corpus
    python3 tools/parity/emacs_oracle.py --generate [PATH]  # capture the fixture corpus from the oracle
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
DEFAULT_CORPUS = REPO_ROOT / "tests" / "parity" / "emacs" / "fixtures" / "corpus.yaml"

INVOKE = "emacs -Q --batch -l <script>"

# The Elisp probe. It sets the buffer, homes point to the fixture's 0-based char offset, turns on
# transient-mark-mode (so mark/region commands behave as they do interactively), applies each op via
# `call-interactively`, then reads state. Every line after the dolist is a pure read — this ordering is
# the non-corruption guarantee. Emacs's native JSON (json-parse-string / json-serialize, 27+) keeps this
# stdlib-only on both ends; a list must be `vconcat`-ed to a vector to serialize as a JSON array.
_ELISP = r"""
(let* ((input (json-parse-string %s :object-type 'alist :array-type 'list))
       (text  (alist-get 'text input))
       (start (alist-get 'point input))
       (ops   (alist-get 'ops input)))
  (with-temp-buffer
    (transient-mark-mode 1)
    (insert text)
    (goto-char (+ (point-min) (or start 0)))
    ;; Model the command loop's `this-command`/`last-command` handling around each command. In batch,
    ;; `call-interactively` does NOT set `this-command` (the real command loop sets it before dispatch);
    ;; only a command that assigns it internally does (e.g. every kill goes through `kill-region`, which
    ;; sets `this-command` to `kill-region`). So we must, before each call: (1) promote the prior
    ;; `this-command` to `last-command`, then (2) set `this-command` to the command about to run. Without
    ;; (2), a non-kill command between two kills (e.g. `forward-char`) leaves the stale `kill-region` in
    ;; `this-command`, so the next kill wrongly ACCUMULATES across it instead of starting a fresh entry;
    ;; without (1), consecutive kills never accumulate at all. Together they make kill-accumulation — and
    ;; any other `last-command`-sensitive behaviour — match an interactive keypress sequence exactly.
    (dolist (op ops)
      (setq last-command this-command)
      (setq this-command (intern op))
      (call-interactively (intern op)))
    (princ (json-serialize
            (list :text (vconcat (split-string (buffer-string) "\n"))
                  :point (1- (point))
                  :mark (if (mark t) (1- (mark t)) :null)
                  :kill (if kill-ring (substring-no-properties (car kill-ring)) :null))))))
"""


class OracleError(RuntimeError):
    """The harness cannot make a trustworthy observation (bad binary, version mismatch, emacs error)."""


def _read_block_fields(block_key: str, fields: tuple[str, ...]) -> dict[str, str]:
    """Pull scalar `fields` from the 2-space-indented `block_key` under `upstreams:` (stdlib parse).

    Mirrors oracle.py's hand parse: locate `upstreams:`, then the `  <block_key>:` line, then read the
    named scalar fields until the block dedents. No PyYAML — the oracle must run with nothing installed.
    """
    lines = UPSTREAMS.read_text(encoding="utf-8").splitlines()
    try:
        start = next(i for i, ln in enumerate(lines) if ln.rstrip() == "upstreams:")
    except StopIteration as exc:  # pragma: no cover - the file always has this key
        raise OracleError(f"no `upstreams:` section in {UPSTREAMS}") from exc
    head = f"  {block_key}:"
    blk = next((i for i in range(start + 1, len(lines)) if lines[i] == head), None)
    if blk is None:
        raise OracleError(f"no `{block_key}:` upstream block")
    found: dict[str, str] = {}
    for ln in lines[blk + 1 :]:
        # Stop at the next upstream (2-space key) or a top-level key (dedent).
        if re.match(r"^  \S", ln) or re.match(r"^\S", ln):
            break
        for f in fields:
            m = re.match(rf"\s*{f}:\s*(\S+)", ln)
            if m and f not in found:
                found[f] = m.group(1)
    missing = [f for f in fields if f not in found]
    if missing:
        raise OracleError(f"{block_key} block missing {'/'.join(missing)}")
    return found


def read_pin() -> dict[str, str]:
    """Extract Emacs's pinned revision + version_label from spec/parity/upstreams.yaml."""
    return _read_block_fields("emacs", ("revision", "version_label"))


def emacs_version() -> str:
    """The first line of `emacs --version`, e.g. 'GNU Emacs 30.2'. Recorded in every run."""
    try:
        out = subprocess.run(
            ["emacs", "--version"], capture_output=True, text=True, check=True
        )
    except (OSError, subprocess.CalledProcessError) as exc:
        raise OracleError(f"cannot run `emacs --version`: {exc}") from exc
    return out.stdout.splitlines()[0].strip()


def _pin_number(version_label: str) -> str:
    """The bare version number from the pin label: 'emacs-30.2' -> '30.2'. `emacs --version` reports
    'GNU Emacs 30.2', so the number (not the label's `emacs-` prefix) is what we match."""
    return re.sub(r"^emacs-", "", version_label)


def assert_pin(version_line: str, pin: dict[str, str]) -> None:
    """Refuse to observe through a binary that is not the pinned version (contract: warn/refuse)."""
    number = _pin_number(pin["version_label"])
    if not re.search(rf"\b{re.escape(number)}\b", version_line):
        raise OracleError(
            f"emacs version mismatch: `{version_line}` is not the pinned {pin['version_label']} "
            f"(spec/parity/upstreams.yaml emacs revision {pin['revision']}). "
            "Refusing: a fixture captured through the wrong binary is not evidence."
        )


def run_emacs(text: str, ops: list[str], point: int = 0) -> dict:
    """Run the pinned Emacs on `text`, apply each op in `ops` via call-interactively, return the state.

    Returns {text:[lines], point:int(0-based char), mark:int|None, kill:str|None, emacs_version}.
    The read happens strictly after the commands settle — see the module docstring for why that is the
    whole point. `point` homes the caret to a 0-based char offset before the ops (Emacs buffers open at
    point-min; a fixture that must start elsewhere records it explicitly rather than prefixing a motion).
    """
    payload = json.dumps({"text": text, "ops": ops, "point": point})
    # Embed the payload as an Elisp string literal (json.dumps already produced a valid one).
    src = _ELISP % json.dumps(payload)
    with tempfile.NamedTemporaryFile(
        "w", suffix=".el", delete=False, encoding="utf-8"
    ) as fh:
        fh.write(src)
        script = fh.name
    try:
        proc = subprocess.run(
            ["emacs", "-Q", "--batch", "-l", script],
            capture_output=True,
            text=True,
        )
    finally:
        Path(script).unlink(missing_ok=True)
    if proc.returncode != 0 or not proc.stdout:
        raise OracleError(
            f"emacs failed (rc={proc.returncode}) on ops={ops!r}: "
            f"{proc.stderr.strip() or '<no stderr>'}"
        )
    raw = json.loads(proc.stdout)
    return {
        "text": raw["text"],
        "point": raw["point"],
        "mark": raw["mark"] if raw["mark"] is not None else None,
        "kill": raw["kill"] if raw["kill"] is not None else None,
        "emacs_version": emacs_version(),
    }


# --- The fixture corpus: ALREADY-IMPLEMENTED ruse Emacs-profile ops. `expect` is captured from the
#     oracle, never hand-written; the (text, ops, point) here are the only human-authored part. Every
#     op name is an Emacs command that the ruse M-x registry (emacs_command_by_name) also resolves, so
#     the same fixture drives both editors. ASCII only for the seed (char offset == byte offset); see
#     the module docstring's scope note.
FIXTURES: list[dict] = [
    # --- motion (point only; text/mark/kill unchanged) ------------------------------------------------
    {"name": "forward_char", "text": "hello", "ops": ["forward-char"]},
    {"name": "forward_char_twice", "text": "hello", "ops": ["forward-char", "forward-char"]},
    {"name": "backward_char", "text": "hello", "ops": ["backward-char"], "point": 3},
    {"name": "forward_word", "text": "foo bar baz", "ops": ["forward-word"]},
    {"name": "backward_word", "text": "foo bar baz", "ops": ["backward-word"], "point": 11},
    {"name": "move_end_of_line", "text": "hello world", "ops": ["move-end-of-line"]},
    {"name": "move_beginning_of_line", "text": "hello world", "ops": ["move-beginning-of-line"], "point": 6},
    {"name": "beginning_of_buffer", "text": "alpha\nbeta\ngamma", "ops": ["beginning-of-buffer"], "point": 12},
    {"name": "end_of_buffer", "text": "alpha\nbeta\ngamma", "ops": ["end-of-buffer"]},
    {"name": "next_line", "text": "alpha\nbeta\ngamma", "ops": ["next-line"]},
    {"name": "previous_line", "text": "alpha\nbeta\ngamma", "ops": ["previous-line"], "point": 12},
    # --- deletion / kill (text + kill-ring) -----------------------------------------------------------
    {"name": "delete_char", "text": "hello", "ops": ["delete-char"]},
    {"name": "kill_line", "text": "hello world", "ops": ["kill-line"], "point": 6},
    {"name": "kill_word", "text": "foo bar baz", "ops": ["kill-word"]},
    {"name": "kill_word_from_mid", "text": "foobar baz", "ops": ["kill-word"], "point": 3},
    # --- mark / region: set-mark, move, then a region op (text + point + mark + kill) -----------------
    {"name": "set_mark_only", "text": "hello world", "ops": ["set-mark-command"], "point": 2},
    {
        "name": "kill_region",
        "text": "hello world",
        "ops": ["set-mark-command", "forward-char", "forward-char", "kill-region"],
        "point": 2,
    },
    {
        "name": "kill_region_word",
        "text": "foo bar baz",
        "ops": ["set-mark-command", "forward-word", "kill-region"],
    },
    {
        "name": "copy_region_keeps_text",
        "text": "abcdef",
        "ops": ["set-mark-command", "forward-char", "forward-char", "forward-char", "kill-ring-save"],
    },
    {
        "name": "exchange_point_and_mark",
        "text": "abcdef",
        "ops": ["set-mark-command", "forward-char", "forward-char", "forward-char", "exchange-point-and-mark"],
    },
    # --- yank: kill then reinsert (unnamed register round-trip) ---------------------------------------
    {
        "name": "kill_word_then_yank",
        "text": "foo bar",
        "ops": ["kill-word", "move-end-of-line", "yank"],
    },
    {
        "name": "copy_region_then_yank",
        "text": "abc",
        "ops": ["set-mark-command", "move-end-of-line", "kill-ring-save", "yank"],
    },
    # --- composite: kill-region then yank elsewhere (cross-command register state) --------------------
    {
        "name": "kill_region_then_yank_at_end",
        "text": "abcdef",
        "ops": ["set-mark-command", "forward-char", "forward-char", "forward-char", "kill-region", "move-end-of-line", "yank"],
    },
    # === EXPANSION 1: deeper semantics of already-shipped commands ==================================
    # --- multi-line kill-line: the classic Emacs subtlety. `kill-line` at end-of-line kills the
    #     NEWLINE (joining the next line), not nothing; from beginning-of-line it kills to EOL. -------
    {"name": "kill_line_from_bol", "text": "hello world", "ops": ["kill-line"]},
    {"name": "kill_line_at_eol", "text": "foo\nbar", "ops": ["kill-line"], "point": 3},
    {"name": "kill_line_whole_then_join", "text": "foo\nbar", "ops": ["kill-line", "kill-line"]},
    # --- kill ACCUMULATION: consecutive kills append onto one kill-ring entry; a non-kill command in
    #     between BREAKS the run so the next kill starts a fresh entry (Emacs `last-command` semantics). --
    {"name": "kill_word_accumulate", "text": "foo bar", "ops": ["kill-word", "kill-word"]},
    {
        "name": "kill_accumulate_breaks_on_move",
        "text": "foo bar baz",
        "ops": ["kill-word", "forward-char", "kill-word"],
    },
    # --- delete-char has no EOL boundary: at end-of-line it deletes the newline (crosses lines). -----
    {"name": "delete_char_at_eol", "text": "foo\nbar", "ops": ["delete-char"], "point": 3},
    {"name": "delete_char_twice", "text": "hello", "ops": ["delete-char", "delete-char"]},
    # --- word motion depth: repeated forward-word, backward-word from mid-word. ----------------------
    {"name": "forward_word_twice", "text": "one two three", "ops": ["forward-word", "forward-word"]},
    {"name": "backward_word_from_mid", "text": "foo bar baz", "ops": ["backward-word"], "point": 6},
    # --- move-end-of-line / next-line on a multi-line buffer (between-char end, curswant). -----------
    {"name": "end_of_line_multiline", "text": "foo\nbar", "ops": ["move-end-of-line"]},
    {"name": "next_line_then_end", "text": "alpha\nbeta\ngamma", "ops": ["next-line", "move-end-of-line"]},
    # --- kill-region with mark AFTER point (backward region): order-independent [min,max). -----------
    {
        "name": "kill_region_backward",
        "text": "hello world",
        "ops": ["set-mark-command", "backward-char", "backward-char", "kill-region"],
        "point": 4,
    },
    # --- yank after a kill-line, reinserted at beginning-of-line. ------------------------------------
    {
        "name": "kill_line_then_yank_at_bol",
        "text": "hello world",
        "ops": ["kill-line", "move-beginning-of-line", "yank"],
    },
    # === EXPANSION 2: forward charts — commands the M-x registry does not resolve yet. These surface
    #     as registry gaps (findings, not failures) that pin the next slices' targets. ---------------
    {"name": "transpose_chars", "text": "abc", "ops": ["transpose-chars"], "point": 1},
    {"name": "capitalize_word", "text": "foo bar", "ops": ["capitalize-word"]},
    # === EXPANSION 3: more discrete editing commands (registry gaps → the next slices' targets). Each is a
    #     pure-editing command that captures cleanly in batch (no minibuffer/char read). ----------------
    # whitespace: back-to-indentation (M-m), just-one-space (M-SPC), delete-horizontal-space (M-\).
    {"name": "back_to_indentation", "text": "  foo", "ops": ["back-to-indentation"], "point": 5},
    {"name": "just_one_space", "text": "foo   bar", "ops": ["just-one-space"], "point": 4},
    {"name": "delete_horizontal_space", "text": "foo   bar", "ops": ["delete-horizontal-space"], "point": 4},
    # open-line (C-o): insert a newline after point, leaving point before it.
    {"name": "open_line", "text": "foobar", "ops": ["open-line"], "point": 3},
    # backward-kill-word (M-DEL): kill the previous word into the ring; consecutive backward kills PREPEND.
    {"name": "backward_kill_word", "text": "foo bar", "ops": ["backward-kill-word"], "point": 7},
    {
        "name": "backward_kill_word_accumulate",
        "text": "foo bar baz",
        "ops": ["backward-kill-word", "backward-kill-word"],
        "point": 11,
    },
    # transpose-words (M-t): swap the two words around point, moving past them.
    {"name": "transpose_words", "text": "foo bar", "ops": ["transpose-words"], "point": 3},
    # mark-word (M-@): set the mark at the end of the next word without moving point.
    {"name": "mark_word", "text": "foo bar", "ops": ["mark-word"]},
    # kill-whole-line (C-S-DEL): kill the entire line including its newline, regardless of point column.
    {"name": "kill_whole_line", "text": "foo\nbar", "ops": ["kill-whole-line"], "point": 1},
    # upcase-region / downcase-region (C-x C-u / C-x C-l): recase the active region [point,mark).
    {
        "name": "upcase_region",
        "text": "foo bar",
        "ops": ["set-mark-command", "forward-word", "upcase-region"],
    },
    {
        "name": "downcase_region",
        "text": "FOO BAR",
        "ops": ["set-mark-command", "forward-word", "downcase-region"],
    },
    # === EXPANSION 4: paragraph motion, delete-indentation, and coverage of already-shipped region/newline. ===
    {"name": "forward_paragraph", "text": "aa\nbb\n\ncc\ndd", "ops": ["forward-paragraph"]},
    {"name": "backward_paragraph", "text": "aa\nbb\n\ncc\ndd", "ops": ["backward-paragraph"], "point": 11},
    # delete-indentation (M-^): join the current line to the previous, collapsing to one space.
    {"name": "delete_indentation", "text": "foo\n   bar", "ops": ["delete-indentation"], "point": 7},
    # coverage: capitalize-region + newline are already implemented — pin them so a regression is caught.
    {
        "name": "capitalize_region",
        "text": "foo bar",
        "ops": ["set-mark-command", "forward-word", "capitalize-region"],
    },
    {"name": "newline_ret", "text": "foobar", "ops": ["newline"], "point": 3},
    # === EXPANSION 5 (edge-case sweep): word boundaries at PUNCTUATION — the place Emacs `forward-word`/
    #     `backward-word` (syntax-word runs, skipping non-word chars) can diverge from a Vim word motion
    #     (which treats a punctuation run as its own word). These are the sweep's prime bug candidates. -----
    {"name": "forward_word_over_punct", "text": "foo.bar", "ops": ["forward-word"]},
    {"name": "forward_word_leading_punct", "text": "...foo", "ops": ["forward-word"]},
    {"name": "forward_word_at_eob", "text": "foo", "ops": ["forward-word"], "point": 3},
    {"name": "backward_word_over_punct", "text": "foo.bar", "ops": ["backward-word"], "point": 4},
    {"name": "backward_word_from_end_punct", "text": "foo.bar", "ops": ["backward-word"], "point": 7},
    {"name": "backward_word_at_bob", "text": "foo bar", "ops": ["backward-word"]},
    # --- kill-word / backward-kill-word across leading whitespace and punctuation runs. ------------------
    {"name": "kill_word_leading_space", "text": "  foo", "ops": ["kill-word"]},
    {"name": "kill_word_over_punct", "text": "foo.bar", "ops": ["kill-word"], "point": 3},
    {"name": "backward_kill_word_over_punct", "text": "foo.bar", "ops": ["backward-kill-word"], "point": 7},
    {"name": "backward_kill_word_trailing_space", "text": "foo bar ", "ops": ["backward-kill-word"], "point": 8},
    # --- transpose-chars in mid-word (advances past the pair) and at end of a NON-last line (no advance). --
    {"name": "transpose_chars_mid", "text": "abcde", "ops": ["transpose-chars"], "point": 2},
    {"name": "transpose_chars_at_eol_multiline", "text": "ab\ncd", "ops": ["transpose-chars"], "point": 2},
    # --- transpose-words with point INSIDE the separator/second word (Emacs transpose-subr geometry). -----
    {"name": "transpose_words_mid", "text": "foo bar baz", "ops": ["transpose-words"], "point": 4},
    # --- case-word: leading whitespace in the span, mixed-case capitalize/downcase, no-op at end-of-buffer. -
    {"name": "upcase_word_leading_space", "text": "  foo", "ops": ["upcase-word"]},
    {"name": "capitalize_word_mixed_case", "text": "fOO bar", "ops": ["capitalize-word"]},
    {"name": "downcase_word_over_punct", "text": "FOO.BAR", "ops": ["downcase-word"]},
    {"name": "upcase_word_at_eob", "text": "foo", "ops": ["upcase-word"], "point": 3},
    # --- open-line at end-of-line (opens a trailing blank) and just-one-space with NO surrounding space. ---
    {"name": "open_line_at_eol", "text": "foo", "ops": ["open-line"], "point": 3},
    {"name": "just_one_space_none", "text": "foobar", "ops": ["just-one-space"], "point": 3},
    {"name": "just_one_space_leading", "text": "   foo", "ops": ["just-one-space"]},
    {"name": "delete_horizontal_space_leading", "text": "   foo", "ops": ["delete-horizontal-space"]},
    # --- delete-indentation onto an EMPTY previous line (fixup collapses to no space, lands at bol). -------
    {"name": "delete_indentation_empty_prev", "text": "\n   bar", "ops": ["delete-indentation"], "point": 4},
    # --- kill-line on an empty line kills just the newline (joins up). -----------------------------------
    {"name": "kill_line_on_empty_line", "text": "\nfoo", "ops": ["kill-line"]},
    # --- composites: kill ACCUMULATION feeding a yank; kill-line then yank in place; copy then recase. ----
    {
        "name": "kill_two_words_then_yank",
        "text": "foo bar baz",
        "ops": ["kill-word", "kill-word", "move-end-of-line", "yank"],
    },
    {"name": "kill_line_mid_then_yank", "text": "hello world", "ops": ["kill-line", "yank"], "point": 6},
    {
        "name": "copy_region_then_upcase_region",
        "text": "foo bar",
        "ops": ["set-mark-command", "forward-word", "kill-ring-save", "upcase-region"],
    },
    {
        "name": "exchange_point_and_mark_twice",
        "text": "abcdef",
        "ops": ["set-mark-command", "forward-char", "forward-char", "forward-char", "exchange-point-and-mark", "exchange-point-and-mark"],
    },
    # --- `_` is a NON-word char in Emacs fundamental-mode (symbol syntax), so `foo_bar` is two words. These
    #     lock in the two-class Emacs word semantics for the family that spans EmacsWordFwd/EmacsWordBack. ---
    {"name": "forward_word_stops_at_underscore", "text": "foo_bar", "ops": ["forward-word"]},
    {"name": "backward_word_stops_at_underscore", "text": "foo_bar", "ops": ["backward-word"], "point": 7},
    {"name": "kill_word_stops_at_underscore", "text": "foo_bar", "ops": ["kill-word"]},
    {"name": "upcase_word_stops_at_underscore", "text": "foo_bar", "ops": ["upcase-word"]},
    # --- backward-kill-word crossing a punctuation run (backward-word skips `.` then over `foo`). ----------
    {"name": "backward_kill_word_cross_punct", "text": "foo.bar", "ops": ["backward-kill-word"], "point": 4},
    # --- transpose-words / mark-word with punctuation between the words (Emacs two-class geometry). --------
    {"name": "transpose_words_over_punct", "text": "foo.bar", "ops": ["transpose-words"], "point": 3},
    {"name": "mark_word_over_leading_punct", "text": ".foo", "ops": ["mark-word"]},
    # === ROUND 2 (edge-case sweep): line/indent motion, mid-word case, transpose at buffer end, whole-line
    #     and horizontal-space collapse at boundaries. Prime candidate: transpose-words at end-of-buffer. ---
    # --- move-beginning/end-of-line on an EMPTY line and at bol/eol (non-modal, no boundary surprises). ----
    {"name": "beginning_of_line_on_empty_line", "text": "foo\n\nbar", "ops": ["move-beginning-of-line"], "point": 4},
    {"name": "end_of_line_on_empty_line", "text": "foo\n\nbar", "ops": ["move-end-of-line"], "point": 4},
    {"name": "end_of_line_from_bol", "text": "hello", "ops": ["move-end-of-line"]},
    # --- back-to-indentation (M-m): all-blank line (lands at end of indent), mid-indent, tabs, no indent. --
    {"name": "back_to_indentation_all_blank", "text": "   ", "ops": ["back-to-indentation"]},
    {"name": "back_to_indentation_from_mid_indent", "text": "    foo", "ops": ["back-to-indentation"], "point": 6},
    {"name": "back_to_indentation_tabs", "text": "\t\tfoo", "ops": ["back-to-indentation"], "point": 5},
    {"name": "back_to_indentation_no_indent", "text": "foo", "ops": ["back-to-indentation"], "point": 2},
    # --- case-word from MID-WORD: Emacs recases POINT..word-end, NOT the whole word (the flagged subtlety). -
    {"name": "upcase_word_mid_word", "text": "foobar", "ops": ["upcase-word"], "point": 3},
    {"name": "downcase_word_mid_word", "text": "FOOBAR", "ops": ["downcase-word"], "point": 3},
    {"name": "capitalize_word_mid_word", "text": "foobar", "ops": ["capitalize-word"], "point": 3},
    # --- transpose-chars at END-OF-BUFFER: swaps the last two chars, point stays at eob. -------------------
    {"name": "transpose_chars_at_eob", "text": "abc", "ops": ["transpose-chars"], "point": 3},
    # (transpose-words at end-of-buffer is NOT fixtured: Emacs signals "Don't have two things to transpose"
    #  when there is no following word, which aborts batch capture — an error the corpus can't encode.)
    # --- kill-line at EOL joining a line whose indentation is preserved (only the newline is killed). ------
    {"name": "kill_line_at_eol_join_indent", "text": "foo\n   bar", "ops": ["kill-line"], "point": 3},
    # --- kill-whole-line: on the LAST line (no trailing newline) and a MIDDLE line (takes the newline). ----
    {"name": "kill_whole_line_last_line", "text": "foo\nbar", "ops": ["kill-whole-line"], "point": 5},
    {"name": "kill_whole_line_middle", "text": "aa\nbb\ncc", "ops": ["kill-whole-line"], "point": 4},
    # --- open-line (C-o) at beginning-of-line: inserts a newline, point stays before it. ------------------
    {"name": "open_line_at_bol", "text": "foo", "ops": ["open-line"]},
    # --- just-one-space / delete-horizontal-space over TABS and mixed space+tab runs. --------------------
    {"name": "just_one_space_tabs", "text": "foo\t\tbar", "ops": ["just-one-space"], "point": 4},
    {"name": "delete_horizontal_space_mixed", "text": "foo \t bar", "ops": ["delete-horizontal-space"], "point": 4},
    # --- delete-indentation with NO trailing whitespace on the prev line (still collapses to one space). ---
    {"name": "delete_indentation_no_trailing", "text": "foo\nbar", "ops": ["delete-indentation"], "point": 4},
    # --- backward-kill-word from END over a TRAILING punctuation run then the word (two-class backward). ---
    {"name": "backward_kill_word_trailing_punct", "text": "foo...", "ops": ["backward-kill-word"], "point": 6},
    # --- mark-word at end-of-buffer: forward-word cannot advance, mark lands at point (degenerate region). -
    {"name": "mark_word_at_eob", "text": "foo", "ops": ["mark-word"], "point": 3},
]


def _entry(spec: dict, state: dict) -> dict:
    entry = {"name": spec["name"], "text": spec["text"], "ops": spec["ops"]}
    # Emit `point` only for fixtures that start off point-min, so the corpus stays byte-identical for the
    # start-at-0 majority and records exactly which runs were homed elsewhere.
    if spec.get("point", 0):
        entry["point"] = spec["point"]
    entry["expect"] = {
        "text": state["text"],
        "point": state["point"],
        "mark": state["mark"],
        "kill": state["kill"],
    }
    return entry


def generate(path: Path) -> int:
    """Capture every fixture's `expect` from the oracle and write the corpus as JSON-in-YAML."""
    pin = read_pin()
    version_line = emacs_version()
    assert_pin(version_line, pin)

    fixtures = [
        _entry(spec, run_emacs(spec["text"], spec["ops"], spec.get("point", 0)))
        for spec in FIXTURES
    ]

    corpus = {
        "version": 1,
        "generator": "tools/parity/emacs_oracle.py",
        "note": (
            "GENERATED — every `expect` was captured from the pinned Emacs oracle via "
            "call-interactively, never hand-written. Regenerate with "
            "`python3 tools/parity/emacs_oracle.py --generate`. point/mark are 0-based CHARACTER "
            "offsets; the seed corpus is ASCII so char == byte and the ruse comparator (byte offsets) "
            "compares them directly. Consecutive kills accumulate onto one kill-ring entry (the oracle "
            "threads `last-command` like the real command loop), so multi-kill fixtures are faithful. "
            "JSON is a subset of YAML 1.2, so this .yaml parses on both ends."
        ),
        "oracle": {
            "editor": "emacs",
            "invoke": INVOKE,
            "emacs_version": version_line,
            "pin_version_label": pin["version_label"],
            "pin_revision": pin["revision"],
            "captured_observables": ["text", "point", "mark", "kill"],
            "ruse_compare_observables": ["text", "point", "mark", "kill"],
            "kill_note": (
                "`kill` is the head of Emacs's kill-ring (car kill-ring); it maps to ruse's single "
                "unnamed register (D-026). Emacs's full ring is out of scope for the seed corpus."
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
    version_line = emacs_version()
    print(f"emacs oracle selftest — {version_line} (pin {pin['version_label']})")
    try:
        assert_pin(version_line, pin)
    except OracleError as exc:
        _fail(str(exc))
        return 1

    failures = 0

    # 1. IDENTITY — no ops must not perturb text, point, mark, or kill. A harness that mutates on
    #    observation (the core hazard) fails here first.
    ident = run_emacs("hello\nworld", [])
    if ident["text"] != ["hello", "world"]:
        _fail(f"identity: text changed on ops=[] -> {ident['text']}")
        failures += 1
    if ident["point"] != 0:
        _fail(f"identity: point moved on ops=[] -> {ident['point']}")
        failures += 1
    if ident["mark"] is not None or ident["kill"] is not None:
        _fail(f"identity: mark/kill non-nil on ops=[] -> mark={ident['mark']} kill={ident['kill']!r}")
        failures += 1

    # 2. DETERMINISM — the same run twice must yield identical observations. Non-determinism means
    #    shared state leaked between processes (the global-var hazard the fresh process guards).
    a = run_emacs("foo bar baz", ["kill-word"])
    b = run_emacs("foo bar baz", ["kill-word"])
    for k in ("text", "point", "mark", "kill"):
        if a[k] != b[k]:
            _fail(f"determinism: {k} differs across identical runs: {a[k]!r} != {b[k]!r}")
            failures += 1

    # 3. KILL-RING ISOLATION — the fixture-to-fixture leak that mandates a fresh process. A run whose
    #    text produces NO kill must observe an EMPTY kill-ring, even right after a run that killed. If
    #    this fails, processes are being reused and every kill observable is suspect.
    run_emacs("foo bar", ["kill-word"])  # populates a kill-ring — in its OWN process
    clean = run_emacs("hello", ["forward-char"])  # no kill here
    if clean["kill"] is not None:
        _fail(f"isolation: kill-ring leaked across processes -> {clean['kill']!r}")
        failures += 1

    # 3b. KILL ACCUMULATION — the `last-command` threading (see run_emacs) must make consecutive kills
    #     APPEND onto one kill-ring entry, exactly as an interactive keypress sequence does. Two
    #     `kill-word`s on "foo bar" leave "" and a single accumulated kill "foo bar"; without threading
    #     the second kill would push a separate " bar" entry and the head would be " bar", not "foo bar".
    accum = run_emacs("foo bar", ["kill-word", "kill-word"])
    if accum["kill"] != "foo bar":
        _fail(f"accumulation: consecutive kills did not append -> kill={accum['kill']!r} (want 'foo bar')")
        failures += 1

    # 3c. KILL ACCUMULATION BREAKS on an intervening non-kill command. `kill-word`, then `forward-char`
    #     (not a kill), then `kill-word` must leave the kill-ring head as JUST the second kill ("bar"), not
    #     the accumulated "foobar". This only holds because the probe sets `this-command` per command; if
    #     it relied on `call-interactively` alone, the stale `kill-region` would leak across forward-char
    #     and the run would wrongly keep accumulating (the exact batch artifact this guards against).
    broke = run_emacs("foo bar baz", ["kill-word", "forward-char", "kill-word"])
    if broke["kill"] != "bar":
        _fail(f"accumulation: a non-kill did not break the run -> kill={broke['kill']!r} (want 'bar')")
        failures += 1

    # 4. KNOWN OPS — hand-verified expectations. If the oracle disagrees, it is LYING and no fixture
    #    recorded through it can be trusted. Answers are known independently of this harness.
    known = [
        (
            "forward-char on 'hello' -> point 1",
            "hello",
            ["forward-char"],
            0,
            lambda s: s["text"] == ["hello"] and s["point"] == 1,
        ),
        (
            "delete-char on 'hello' -> 'ello'",
            "hello",
            ["delete-char"],
            0,
            lambda s: s["text"] == ["ello"] and s["point"] == 0,
        ),
        (
            "kill-word on 'foo bar' -> 'bar', kill 'foo'",
            "foo bar",
            ["kill-word"],
            0,
            lambda s: s["text"] == [" bar"] and s["kill"] == "foo",
        ),
        (
            "set-mark + fwd*2 + kill-region on 'hello world' -> 'heo world', kill 'll'",
            "hello world",
            ["set-mark-command", "forward-char", "forward-char", "kill-region"],
            2,
            lambda s: s["text"] == ["heo world"] and s["point"] == 2 and s["kill"] == "ll",
        ),
        (
            "set-mark + fwd*3 + exchange-point-and-mark on 'abcdef' -> point 0, mark 3",
            "abcdef",
            ["set-mark-command", "forward-char", "forward-char", "forward-char", "exchange-point-and-mark"],
            0,
            lambda s: s["point"] == 0 and s["mark"] == 3 and s["text"] == ["abcdef"],
        ),
    ]
    for label, text, ops, point, ok in known:
        state = run_emacs(text, ops, point)
        if not ok(state):
            _fail(
                f"known-op disagreement: {label} — got text={state['text']} "
                f"point={state['point']} mark={state['mark']} kill={state['kill']!r}"
            )
            failures += 1

    if failures:
        print(f"emacs oracle selftest FAILED ({failures} check(s)) — the corpus is NOT trustworthy.")
        return 1
    print("emacs oracle selftest PASSED — identity, determinism, isolation, and known ops all hold.")
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
