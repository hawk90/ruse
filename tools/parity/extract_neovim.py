#!/usr/bin/env python3
"""parity extract (neovim) — turn the pinned upstream into a machine-derived inventory.

This is the piece the spec never had: an EVIDENCE layer under the spec. Humans may not author the
enumeration (that is how an entire dimension — the per-mode keymap namespaces — went missing while
every governance gate stayed green). Humans author the INTERPRETATION downstream: classification,
concepts, contracts.

Per-surface source of record is declared in spec/parity/upstreams.yaml and is NOT uniform, because
the sources have different precision per item type:

  mode_key    D  runtime/doc/index.txt  — the only source that enumerates keys BY MODE
  map_mode    D  runtime/doc/map.txt    — the map-table: 8 disjoint keymap namespaces
  option      R  nvim_get_all_options_info()   (options.lua = static cross-check)
  ex_command  S  src/nvim/ex_cmds.lua          (the builtin=true API returns 1 item — unusable)
  event       S  src/nvim/auevents.lua         (getcompletion('','event') = cross-check)

Output is spec/parity/inventory/neovim/<surface>.yaml, one file per surface so a revision bump
produces a readable diff. Every item lands as `status: unclassified` — discovery is strict and
mechanical, classification is lazy and human (and is locked at SURFACE granularity by
`gov parity_discovery`, so you cannot classify five keys of a mode and miss that the mode itself
needs a dimension).

REGENERATION IS MERGE, NOT OVERWRITE: human-owned fields (status/ruse/note/superseded_by) are
carried forward by id. Items that vanish upstream are kept and flagged `upstream_gone: true`
rather than deleted, so a bump can never silently drop a surface ruse already committed to.

  python3 tools/parity/extract_neovim.py            # write the inventory
  python3 tools/parity/extract_neovim.py --dry-run  # report counts only
"""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import yaml  # noqa: E402

from rusekit import render, repo  # noqa: E402

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from fetch import cache_dir, load_upstreams  # noqa: E402

EDITOR = "neovim"
OUT_DIR = "spec/parity/inventory/neovim"

# Human-owned fields preserved across regeneration. Everything else is machine-derived and
# overwritten from upstream on every run.
HUMAN_FIELDS = ("status", "ruse", "note", "superseded_by", "surface_locked")

# index.txt section heading -> ruse surface name. The section structure IS the evidence that Vim's
# key namespaces are disjoint per mode; flattening it is what the hand-written catalog did.
# The "EX commands" section is deliberately absent: ex_cmds.lua is that surface's source of record,
# and counting both would double-count. index.txt's ex count is recorded as corroboration instead.
INDEX_SECTIONS = {
    "1 Insert mode":                    "insert",
    "2 Normal mode":                    "normal",
    "2.1 Text objects":                 "text_object",
    "2.2 Window commands":              "normal.window",
    "2.3 Square bracket commands":      "normal.bracket",
    "2.4 Commands starting with 'g'":   "normal.g",
    "2.5 Commands starting with 'z'":   "normal.z",
    "2.6 Operator-pending mode":        "operator_pending",
    "3 Visual mode":                    "visual",
    "4 Command-line editing":           "cmdline",
    "5 Terminal mode":                  "terminal",
}
EX_SECTION = "6 EX commands"

# Surfaces upstream declares but does NOT enumerate as a table. A table-scraper reports these as
# zero keys, which is worse than the hand-written catalog it replaces — so they are declared, the
# prose is captured verbatim as evidence, and the extractor FAILS if any other surface comes back
# empty (an upstream restructure must never look like "this mode has no keys").
#
# Terminal mode is the canonical case and it is not an accident of formatting: its namespace cannot
# be written as a key table because its rule is "everything is forwarded except one prefix". That
# is the unmatched-key policy — the very dimension the hand-written VIM-MODE table lacked — and
# index.txt drops to prose precisely where that dimension is the whole content.
NON_ENUMERABLE = {
    "terminal": "index.txt states the namespace as a forwarding rule, not a key table: all keys "
                "except CTRL-\\ go to the job. Enumeration is impossible by construction; the "
                "policy is the content. Classification must capture it as an unmatched-key policy.",
}

SEC_RE = re.compile(r"^(\d+(?:\.\d+)?)\.?\s+(.+?)\s+\*")


def _slug(key: str) -> str:
    """A stable, YAML/path-safe id fragment. The raw key is always kept in `key:` so the slug never
    has to be reversible — only stable, so classification survives regeneration."""
    s = key.strip()
    s = s.replace("CTRL-", "c_").replace("<", "").replace(">", "")
    out = []
    for ch in s:
        if ch.isalnum() or ch in "-_.":
            out.append(ch)
        else:
            out.append(f"x{ord(ch):02x}")
    return ("".join(out) or "empty")[:48]


# ---- surface extractors ---------------------------------------------------------


# index.txt's entry grammar, verified against the pinned file:
#   tagged entry     `|i_CTRL-A|\tCTRL-A\t\tinsert previously inserted text`
#   untagged entry   `\t\tCTRL-F\t\tnot used ...`          (exactly two leading tabs; unbound key)
#   continuation     `\t\t\t\t...`                          (four leading tabs; wraps the previous desc)
#   column header    `Tag\t\tChar\t\tInsert-mode action\t~` (trailing ~)
# A looser "any line with a tab" rule silently swallows headers, wrapped description fragments and
# the *holy-grail* easter egg as if they were keys — 218 phantom entries out of 924 when tried.
ENTRY_TAGGED = re.compile(r"^\|([^|\t]+)\|\t+(.*)$")
ENTRY_BARE = re.compile(r"^\t\t(?!\t)(.*)$")
CONTINUATION = re.compile(r"^\t{3,}(\S.*)$")
NOTE_COL = re.compile(r"^([12])\s{2,}(.*)$")


def extract_mode_keys(root: str) -> tuple[list[dict], dict]:
    """runtime/doc/index.txt — key entries grouped by the mode section that owns them.

    The section structure is the evidence: upstream enumerates keys BY MODE because the namespaces
    are disjoint. A key belongs to exactly one surface here, which is what makes `mode_key.insert`
    vs `mode_key.operator_pending` a real distinction rather than a flag on one table."""
    path = os.path.join(root, "runtime/doc/index.txt")
    items, section, ex_count = [], None, 0
    seen: set[str] = set()
    current: dict | None = None

    def flush():
        nonlocal current
        if current is not None:
            current["desc"] = (current.get("desc") or "").strip() or None
            items.append(current)
            current = None

    with open(path, encoding="utf-8", errors="replace") as fh:
        for raw in fh:
            line = raw.rstrip("\n")
            m = SEC_RE.match(line)
            if m and "*" in line:
                flush()
                section = f"{m.group(1)} {m.group(2).strip()}"
                continue
            if section == EX_SECTION:
                if ENTRY_TAGGED.match(line) or ENTRY_BARE.match(line):
                    ex_count += 1
                continue
            surface = INDEX_SECTIONS.get(section or "")
            if not surface:
                continue
            if line.rstrip().endswith("~") or not line.strip():
                flush()
                continue

            cont = CONTINUATION.match(line)
            if cont and current is not None:
                # A long key pushes its description onto the next line; otherwise this wraps prose.
                current["desc"] = (current.get("desc") or "") + " " + cont.group(1).strip()
                continue

            mt, mb = ENTRY_TAGGED.match(line), ENTRY_BARE.match(line)
            if not (mt or mb):
                flush()
                continue
            flush()
            tag = mt.group(1) if mt else None
            body = (mt.group(2) if mt else mb.group(1))
            parts = [c for c in body.split("\t")]
            key = parts[0].strip()
            desc = " ".join(p.strip() for p in parts[1:] if p.strip())
            if not key:
                continue
            flags = []
            if (nm := NOTE_COL.match(desc)):
                flags, desc = [nm.group(1)], nm.group(2)
            iid = f"nvim.key.{surface}.{_slug(key)}"
            if iid in seen:          # a few keys are listed twice within one section
                continue
            seen.add(iid)
            current = {"id": iid, "surface": f"mode_key.{surface}", "key": key,
                       "desc": desc, "attestation": ["D"]}
            if tag:
                current["tag"] = tag
            if "1" in flags:
                current["moves_cursor"] = True
            if "2" in flags:
                current["undoable"] = True
        flush()

    # Every declared section must yield keys, or be declared non-enumerable with its prose evidence.
    found = {i["surface"].split(".", 1)[1] for i in items}
    empty = [s for s in INDEX_SECTIONS.values() if s not in found]
    prose = _section_prose(path)
    for surface in empty:
        reason = NON_ENUMERABLE.get(surface)
        if reason is None:
            raise SystemExit(
                f"extract: surface '{surface}' yielded 0 keys and is not declared in "
                f"NON_ENUMERABLE. Upstream restructured, or the entry grammar drifted — "
                f"investigate rather than shipping an empty surface.")
        items.append({
            "id": f"nvim.key.{surface}.__surface__",
            "surface": f"mode_key.{surface}",
            "enumerable": False,
            "reason": reason,
            "evidence_prose": prose.get(surface),
            "attestation": ["D"],
        })
    return items, {"index_txt_ex_section_entries": ex_count,
                   "non_enumerable_surfaces": sorted(empty)}


def _section_prose(path: str) -> dict[str, str]:
    """Verbatim prose body of each declared section, for surfaces that are not table-enumerable.
    Recording it keeps the machine the source of the FACT while leaving the reading to a human."""
    out: dict[str, list[str]] = {}
    section = None
    for raw in open(path, encoding="utf-8", errors="replace"):
        line = raw.rstrip("\n")
        m = SEC_RE.match(line)
        if m and "*" in line:
            section = INDEX_SECTIONS.get(f"{m.group(1)} {m.group(2).strip()}")
            continue
        if section and line.strip() and not line.startswith(("\t", "|", "=", "-")):
            out.setdefault(section, []).append(line.strip())
    return {k: " ".join(v) for k, v in out.items()}


def extract_map_modes(root: str) -> list[dict]:
    """runtime/doc/map.txt map-table — the 8 disjoint keymap namespaces. This is upstream's own
    formal statement that a mode is a namespace, not a flag on one state machine."""
    path = os.path.join(root, "runtime/doc/map.txt")
    text = open(path, encoding="utf-8", errors="replace").read()
    m = re.search(r"Mode\s+\|(.+?)\|\s*~\s*\n(.*?)\n\s*\n", text, re.S)
    modes: list[str] = []
    if m:
        modes = [c.strip() for c in m.group(1).split("|") if c.strip()]
    rows = re.findall(r"^([a-z]?\[nore\]map[!]?)\s*\|(.+)$", text, re.M)
    items = []
    for i, mode in enumerate(modes):
        cmds = [cmd for cmd, cells in rows
                if (parts := [c.strip() for c in cells.split("|")]) and i < len(parts)
                and parts[i] == "yes"]
        items.append({
            "id": f"nvim.mapmode.{mode.lower()}",
            "surface": "map_mode",
            "mode": mode,
            "map_commands": cmds,
            "attestation": ["D"],
        })
    return items


def _lua_records(path: str, key: str) -> list[str]:
    """Names of records in a nvim *.lua table, e.g. every `command = 'abclear'`."""
    text = open(path, encoding="utf-8", errors="replace").read()
    return re.findall(rf"^\s*{key}\s*=\s*'([^']+)'", text, re.M)


def extract_ex_commands(root: str) -> list[dict]:
    """src/nvim/ex_cmds.lua — source of record. The runtime API for builtins is unreliable."""
    path = os.path.join(root, "src/nvim/ex_cmds.lua")
    text = open(path, encoding="utf-8", errors="replace").read()
    out, seen = [], set()
    for m in re.finditer(r"^\s*command\s*=\s*'([^']+)',\s*$", text, re.M):
        name = m.group(1)
        tail = text[m.end():m.end() + 400]
        addr = re.search(r"addr_type\s*=\s*'([A-Z_]+)'", tail)
        if name in seen:
            continue
        seen.add(name)
        out.append({"id": f"nvim.ex.{_slug(name)}", "surface": "ex_command", "command": name,
                    "addr_type": addr.group(1) if addr else None, "attestation": ["S"]})
    return out


def extract_events(root: str) -> list[dict]:
    """src/nvim/auevents.lua — autocommand events."""
    path = os.path.join(root, "src/nvim/auevents.lua")
    text = open(path, encoding="utf-8", errors="replace").read()
    block = re.search(r"events\s*=\s*\{(.*?)\n  \}", text, re.S)
    body = block.group(1) if block else ""
    out = []
    for m in re.finditer(r"^\s*(\w+)\s*=\s*(true|false),(?:\s*--\s*(.*))?$", body, re.M):
        out.append({"id": f"nvim.event.{m.group(1).lower()}", "surface": "event",
                    "event": m.group(1), "window_local": m.group(2) == "true",
                    "desc": (m.group(3) or "").strip() or None, "attestation": ["S"]})
    return out


# ---- runtime introspection (source of record for options; cross-check elsewhere) ----


def nvim_probe() -> dict | None:
    """Headless `nvim -u NONE` self-report. Returns None if no usable nvim is on PATH — the
    extractor still produces a static inventory, and coverage.yaml records that R was unavailable."""
    lua = (
        "local o = vim.api.nvim_get_all_options_info() "
        "local opts = {} for k, v in pairs(o) do "
        "opts[#opts+1] = {name=k, type=v.type, scope=v.scope, default=v.default, "
        "abbreviation=v.shortname, was_set=v.was_set} end "
        "io.stdout:write(vim.json.encode({version=vim.version(), options=opts, "
        "ex_commands=vim.fn.getcompletion('', 'command'), "
        "events=vim.fn.getcompletion('', 'event')}))"
    )
    try:
        p = subprocess.run(["nvim", "--headless", "-u", "NONE", "-i", "NONE",
                            "--cmd", f"lua {lua}", "--cmd", "qa!"],
                           capture_output=True, text=True, timeout=60)
    except (FileNotFoundError, subprocess.SubprocessError):
        return None
    raw = (p.stdout or "").strip()
    start = raw.find("{")
    if start < 0:
        return None
    try:
        return json.loads(raw[start:])
    except json.JSONDecodeError:
        return None


def extract_options(root: str, probe: dict | None) -> tuple[list[dict], dict]:
    """Options come from the RUNTIME (a binary knows its own defaults); options.lua cross-checks."""
    static_names = set(_lua_records(os.path.join(root, "src/nvim/options.lua"), "full_name"))
    items = []
    if probe:
        for o in sorted(probe.get("options") or [], key=lambda x: x["name"]):
            name = o["name"]
            items.append({
                "id": f"nvim.opt.{_slug(name)}", "surface": "option", "option": name,
                "type": o.get("type"), "scope": o.get("scope"),
                "abbreviation": o.get("abbreviation") or None,
                "default": o.get("default"),
                "attestation": ["R", "S"] if name in static_names else ["R"],
            })
    else:
        for name in sorted(static_names):
            items.append({"id": f"nvim.opt.{_slug(name)}", "surface": "option",
                          "option": name, "attestation": ["S"]})
    runtime_names = {o["name"] for o in (probe.get("options") if probe else []) or []}
    return items, {
        "static_options": len(static_names),
        "runtime_options": len(runtime_names),
        "runtime_only": sorted(runtime_names - static_names)[:20],
        "static_only": sorted(static_names - runtime_names)[:20],
    }


# ---- merge + write --------------------------------------------------------------


def load_existing(surface_file: str) -> dict[str, dict]:
    p = repo.path(OUT_DIR, surface_file)
    if not os.path.isfile(p):
        return {}
    doc = yaml.safe_load(open(p, encoding="utf-8")) or {}
    return {i["id"]: i for i in (doc.get("items") or []) if isinstance(i, dict) and i.get("id")}


def merge(fresh: list[dict], prior: dict[str, dict]) -> tuple[list[dict], int, int]:
    """Machine fields from upstream; human fields carried forward. Vanished ids are RETAINED and
    flagged — a revision bump must never silently delete a surface ruse already reasoned about."""
    out, fresh_ids = [], {i["id"] for i in fresh}
    for item in fresh:
        old = prior.get(item["id"], {})
        for f in HUMAN_FIELDS:
            if f in old:
                item[f] = old[f]
        item.setdefault("status", "unclassified")
        out.append(item)
    gone = 0
    for iid, old in sorted(prior.items()):
        if iid not in fresh_ids:
            old["upstream_gone"] = True
            out.append(old)
            gone += 1
    return out, len(fresh_ids - set(prior)), gone


def write_surface(surface_file: str, title: str, source: str, items: list[dict],
                  revision: str, label: str, dry: bool) -> dict:
    prior = load_existing(surface_file)
    merged, added, gone = merge(items, prior)
    unclassified = sum(1 for i in merged if i.get("status") == "unclassified")
    stats = {"total": len(merged), "added": added, "upstream_gone": gone,
             "unclassified": unclassified}
    if dry:
        return stats
    doc = {
        "version": 1,
        "generated": True,
        "generator": "tools/parity/extract_neovim.py",
        "upstream": EDITOR,
        "revision": revision,
        "version_label": label,
        "surface": title,
        "source_of_record": source,
        "items": merged,
    }
    os.makedirs(repo.path(OUT_DIR), exist_ok=True)
    header = (
        f"# GENERATED — do not hand-edit item enumeration. Regenerate:\n"
        f"#   python3 tools/parity/fetch.py {EDITOR} && python3 tools/parity/extract_neovim.py\n"
        f"# Human-owned fields ({', '.join(HUMAN_FIELDS)}) ARE preserved across regeneration; the\n"
        f"# enumeration is not. `status: unclassified` is legitimate and expected — discovery is\n"
        f"# strict, classification is lazy (and locked per surface by `ruse gov parity_discovery`).\n"
    )
    with open(repo.path(OUT_DIR, surface_file), "w", encoding="utf-8") as fh:
        fh.write(header)
        yaml.safe_dump(doc, fh, sort_keys=False, allow_unicode=True, width=120)
    return stats


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(prog="parity extract_neovim")
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args(argv if argv is not None else sys.argv[1:])

    ups = (load_upstreams().get("upstreams") or {}).get(EDITOR) or {}
    revision, label = ups.get("revision", "?"), ups.get("version_label", "?")
    root = cache_dir(EDITOR)
    if not os.path.isdir(os.path.join(root, "runtime", "doc")):
        render.fail(f"upstream cache missing — run: python3 tools/parity/fetch.py {EDITOR}")
        return 1

    render.heading(f"parity extract — {EDITOR} @ {label} ({revision[:12]})")

    probe = nvim_probe()
    if probe:
        v = probe.get("version") or {}
        binver = f"v{v.get('major')}.{v.get('minor')}.{v.get('patch')}"
        match = "matches pin" if binver == label else f"DIFFERS from pin {label}"
        render.field("Runtime probe", f"nvim {binver} — {match}")
    else:
        render.field("Runtime probe", "unavailable (no usable nvim on PATH) — static sources only")

    keys, key_extra = extract_mode_keys(root)
    options, opt_extra = extract_options(root, probe)
    surfaces = [
        ("mode_key.yaml", "mode_key", "D: runtime/doc/index.txt", keys),
        ("map_mode.yaml", "map_mode", "D: runtime/doc/map.txt", extract_map_modes(root)),
        ("option.yaml", "option", "R: nvim_get_all_options_info() (S: src/nvim/options.lua)", options),
        ("ex_command.yaml", "ex_command", "S: src/nvim/ex_cmds.lua", extract_ex_commands(root)),
        ("event.yaml", "event", "S: src/nvim/auevents.lua", extract_events(root)),
    ]

    total = 0
    for fname, title, source, items in surfaces:
        st = write_surface(fname, title, source, items, revision, label, args.dry_run)
        total += st["total"]
        extra = f", +{st['added']} new" if st["added"] and load_existing(fname) else ""
        gone = f", {st['upstream_gone']} upstream-gone" if st["upstream_gone"] else ""
        render.bullet(f"{title:<12} {st['total']:>5} items ({st['unclassified']} unclassified{extra}{gone})")

    render.field("Cross-checks", "")
    render.bullet(f"options: runtime {opt_extra['runtime_options']} vs static {opt_extra['static_options']} "
                  f"(delta is a method artifact, not drift)")
    if probe:
        render.bullet(f"ex commands: static-table source-of-record vs runtime completion "
                      f"{len(probe.get('ex_commands') or [])}")
        render.bullet(f"events: static-table source-of-record vs runtime completion "
                      f"{len(probe.get('events') or [])}")
    render.bullet(f"index.txt EX section: {key_extra['index_txt_ex_section_entries']} entries "
                  f"(corroboration only — ex_cmds.lua is the source of record)")

    render.ok(f"{total} upstream items enumerated"
              + (" (dry run — nothing written)" if args.dry_run else f" → {OUT_DIR}/"))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
