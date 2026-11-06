"""Set Change Contract fields from the command line, so nobody hand-edits change.yaml.

`change start` and `change set` share these flags. The contract lives inside `.git` (D-062), where some
editors and sandboxed agents cannot write, so every field an author normally fills is settable here:

  --kind KIND          contract kind
  --goal TEXT          the one-paragraph goal (replaces it)
  --allow PATH         allow_paths entry        (repeatable)
  --forbid PATH        forbid_paths entry       (repeatable)
  --spec-ref ID        affected spec ID, filed by prefix: CAP-* → capabilities, F-*/C-* → requirements,
                       INV-* → invariants, D-* → decisions   (repeatable)
  --non-goal TEXT      non_goals entry          (repeatable)
  --rfc ID             artifacts.rfc            (scalar: replaces)
  --decision ID        artifacts.decision       (scalar: replaces)
  --persistent-format  contracts.persistent_format = true (`--no-persistent-format` sets false): the
                       change alters a stored format (the journal, a save format)
  --protocol           contracts.protocol (plugin/remote protocol), `--no-protocol`
  --public-api         contracts.public_api, `--no-public-api`
  --migration REF      artifacts.migration: where the migration of old data is decided or done (an RFC
                       section, a D-*, a PR); `--migration none` records that none is needed
  --compat-reviewed REF  artifacts.compat_reviewed: where the compatibility review of a `contract` change
                       is recorded (an RFC section, a review comment, a PR); `pr check` requires it

List flags APPEND to what the contract already holds (duplicates are skipped); `change set --replace`
makes each list flag given REPLACE its list instead. Scalars always replace.

Edits are line-level on the template's one-line fields, so the template's comments survive. A field that
was hand-rewritten into a shape this cannot edit in place (e.g. a block list) is rewritten through YAML,
which drops comments; a warning says so.
"""
from __future__ import annotations

import argparse
import json
import os
import re

try:
    import yaml
except ImportError:  # pragma: no cover
    yaml = None

from rusekit import contract, render, repo  # noqa: E402

SPEC_REF_SECTIONS = (("CAP-", "capabilities"), ("INV-", "invariants"), ("F-", "requirements"),
                     ("C-", "requirements"), ("D-", "decisions"))

# contracts.* booleans (a true one holds the change to kind `contract`), and what each one guards
CONTRACT_FLAGS = {"persistent_format": "a stored on-disk format (the journal, a save format)",
                  "protocol": "the plugin or remote protocol",
                  "public_api": "a public API"}

# (section or None for top level, key) for each list flag
LIST_FIELDS = {"allow": (None, "allow_paths"), "forbid": (None, "forbid_paths"),
               "non_goal": (None, "non_goals")}


def add_arguments(ap: argparse.ArgumentParser, *, with_kind: bool = True) -> None:
    if with_kind:
        ap.add_argument("--kind", choices=contract.kind_names() or None,
                        help="declared change kind (see spec/change-kinds.yaml)")
        ap.add_argument("--goal", help="one-line goal (replaces change.yaml's goal)")
    ap.add_argument("--allow", action="append", default=[], metavar="PATH",
                    help="allow_paths entry (repeatable)")
    ap.add_argument("--forbid", action="append", default=[], metavar="PATH",
                    help="forbid_paths entry (repeatable)")
    ap.add_argument("--spec-ref", action="append", default=[], metavar="ID",
                    help="affected spec ID: CAP-*, F-*/C-*, INV-*, D-* (repeatable)")
    ap.add_argument("--non-goal", action="append", default=[], metavar="TEXT",
                    help="non_goals entry (repeatable)")
    ap.add_argument("--rfc", metavar="ID", help="artifacts.rfc (e.g. RFC-0021)")
    ap.add_argument("--decision", metavar="ID", help="artifacts.decision (e.g. D-062)")
    for flag, what in CONTRACT_FLAGS.items():
        ap.add_argument(f"--{flag.replace('_', '-')}", dest=flag,
                        action=argparse.BooleanOptionalAction, default=None,
                        help=f"contracts.{flag}: the change alters {what}")
    ap.add_argument("--migration", metavar="REF",
                    help="artifacts.migration: where old data's migration is decided "
                         "(e.g. \"RFC-0023 §3\"), or `none` when no migration is needed")
    ap.add_argument("--compat-reviewed", metavar="REF",
                    help="artifacts.compat_reviewed: where the compatibility review of a `contract` "
                         "change is recorded (e.g. \"RFC-0006 §4\")")


def spec_section(ref: str) -> str | None:
    for prefix, section in SPEC_REF_SECTIONS:
        if ref.startswith(prefix):
            return section
    return None


# ---- line-level editing -----------------------------------------------------------

_LINE = re.compile(r"^(?P<indent>\s*)(?P<key>[A-Za-z_]+):(?P<sp>\s*)(?P<val>.*?)(?P<cmt>\s+#.*)?$")


def _locate(lines: list[str], section: str | None, key: str) -> int | None:
    """Index of `key:` at top level (section None) or directly inside top-level `section:`."""
    in_section = section is None
    for i, line in enumerate(lines):
        m = _LINE.match(line)
        if not m:
            continue
        indent = len(m["indent"])
        if section is not None:
            if indent == 0:
                in_section = m["key"] == section
                continue
            if in_section and indent == 2 and m["key"] == key:
                return i
        elif indent == 0 and m["key"] == key:
            return i
    return None


def _set_inline(lines: list[str], section: str | None, key: str, value) -> bool:
    """Replace a one-line `key: <flow value>` in place, keeping its trailing comment."""
    i = _locate(lines, section, key)
    if i is None:
        return False
    m = _LINE.match(lines[i])
    val = m["val"].strip()
    if not val or val in (">", "|", ">-", "|-"):
        return False            # a block value: not editable in place
    rendered = json.dumps(value, ensure_ascii=False)
    pad = m["sp"] or " "
    head = f"{m['indent']}{key}:{pad}{rendered}"
    cmt = (m["cmt"] or "").lstrip()
    if cmt:     # keep the template's comment column where it still fits
        col = len(lines[i]) - len(cmt)
        head += " " * max(1, col - len(head)) + cmt
    lines[i] = head
    return True


def _set_goal(lines: list[str], goal: str) -> bool:
    """Replace the folded `goal: >` block (its indented continuation lines) with one paragraph."""
    i = _locate(lines, None, "goal")
    if i is None:
        return False
    j = i + 1
    while j < len(lines) and lines[j].startswith("  "):
        j += 1
    lines[i:j] = ["goal: >", f"  {goal}"]
    return True


def _set_block_list(lines: list[str], key: str, items: list[str]) -> bool:
    """Top-level `key:` followed by `  - item` lines (the template's non_goals shape)."""
    i = _locate(lines, None, key)
    if i is None or _LINE.match(lines[i])["val"].strip():
        return False
    j = i + 1
    while j < len(lines) and lines[j].startswith("  - "):
        j += 1
    lines[i + 1:j] = [f"  - {json.dumps(it, ensure_ascii=False)}" for it in items]
    if not items:
        lines[i] = f"{key}: []"
    return True


def apply(text: str, args, *, replace: bool = False) -> tuple[str, list[str]]:
    """Apply the flags in `args` to contract text. Returns (new text, human-readable changes)."""
    if yaml is None:
        raise RuntimeError("PyYAML is required to edit the contract")
    data = yaml.safe_load(text) or {}
    lines = text.splitlines()
    changes: list[str] = []
    fallback: dict = {}     # (section, key) -> value that could not be edited in place

    def put(section, key, value):
        if key == "non_goals":
            ok = _set_block_list(lines, key, value) or _set_inline(lines, section, key, value)
        else:
            ok = _set_inline(lines, section, key, value)
        if not ok:
            fallback[(section, key)] = value

    def merged(old, new):
        if replace:
            return list(dict.fromkeys(new))
        old = [o for o in (old or []) if not (isinstance(o, str) and o.startswith("<"))]  # template stub
        return list(dict.fromkeys(list(old) + list(new)))

    for flag, (section, key) in LIST_FIELDS.items():
        given = getattr(args, flag, None) or []
        if given:
            value = merged(data.get(key), given)
            put(section, key, value)
            changes.append(f"{key} = {value}")

    refs = getattr(args, "spec_ref", None) or []
    by_section: dict[str, list[str]] = {}
    for ref in refs:
        sec = spec_section(ref)
        if sec is None:
            raise ValueError(f"--spec-ref {ref}: expected CAP-*, F-*, C-*, INV-* or D-* "
                             "(RFCs go in --rfc, decisions-of-record in --decision)")
        by_section.setdefault(sec, []).append(ref)
    affected = data.get("affected") or {}
    for sec, ids in by_section.items():
        value = merged(affected.get(sec), ids)
        put("affected", sec, value)
        changes.append(f"affected.{sec} = {value}")

    for flag, key in (("rfc", "rfc"), ("decision", "decision")):
        v = getattr(args, flag, None)
        if v is not None:
            put("artifacts", key, v)
            changes.append(f"artifacts.{key} = {v}")
    for flag in CONTRACT_FLAGS:
        v = getattr(args, flag, None)
        if v is not None:
            put("contracts", flag, v)
            changes.append(f"contracts.{flag} = {str(v).lower()}")
    mig = getattr(args, "migration", None)
    if mig is not None:
        mig = mig.strip()
        if not mig:
            raise ValueError("--migration: give a reference (an RFC section, D-*, PR) or `none`")
        put("artifacts", "migration", mig)
        changes.append(f"artifacts.migration = {mig}")
    cr = getattr(args, "compat_reviewed", None)
    if cr is not None:
        cr = cr.strip()
        if not cr:
            raise ValueError("--compat-reviewed: give where the review is recorded (an RFC section, a PR)")
        put("artifacts", "compat_reviewed", cr)
        changes.append(f"artifacts.compat_reviewed = {cr}")
    kind = getattr(args, "kind", None)
    if kind:
        put(None, "kind", kind)
        changes.append(f"kind = {kind}")
    goal = getattr(args, "goal", None)
    if goal:
        if not _set_goal(lines, goal):
            fallback[(None, "goal")] = goal
        changes.append("goal updated")

    out = "\n".join(lines) + ("\n" if text.endswith("\n") or not text else "")
    if fallback:
        render.warn("contract has hand-edited fields; rewriting it through YAML (comments dropped)")
        data = yaml.safe_load(out) or {}
        for (section, key), value in fallback.items():
            (data.setdefault(section, {}) if section else data)[key] = value
        out = yaml.safe_dump(data, sort_keys=False, allow_unicode=True)
    return out, changes


# ---- `ruse change set` --------------------------------------------------------------

def main_set(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(
        prog="ruse change set",
        description="Amend an existing Change Contract. List flags append (duplicates skipped); "
                    "with --replace each list flag given replaces its list. Scalars always replace.")
    ap.add_argument("--issue", help="change workspace (default: the active one)")
    ap.add_argument("--replace", action="store_true",
                    help="list flags replace their list instead of appending")
    add_arguments(ap)
    args = ap.parse_args(argv)

    issue = args.issue or repo.active_issue()
    if not issue:
        render.fail("no change workspace; run `ruse change start --issue <id>` first")
        return 1
    cy = contract.contract_path(issue)
    if not os.path.isfile(cy):
        render.fail(f"no contract at {repo.show(cy)} — run `ruse change start --issue {issue}`")
        return 1
    with open(cy, encoding="utf-8") as fh:
        text = fh.read()
    try:
        new, changes = apply(text, args, replace=args.replace)
    except ValueError as e:
        render.fail(str(e))
        return 2
    if not changes:
        render.warn("nothing to set (see `ruse change set --help`)")
        return 0
    tmp = cy + ".tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        fh.write(new)
    os.replace(tmp, cy)
    render.heading("change set")
    render.field("Contract", repo.show(cy))
    for c in changes:
        render.bullet(c)
    render.ok("contract updated")
    return 0
