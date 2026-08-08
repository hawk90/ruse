#!/usr/bin/env python3
"""parity extract_helix — the selection-first census (D-043, role: reference).

WHY THIS UPSTREAM IS PINNED, AND WHY ITS NUMBERS ARE NOT PARITY
---------------------------------------------------------------
Neovim and Emacs are pinned because ruse intends compatibility with them. Helix is not. It is pinned
because three entries in concepts/irreconcilable.yaml#observables — OBS-BARE-MOTION,
OBS-SELECTION-PERSISTENCE, OBS-DOT-REPEAT-UNIT — had their selection-first column ASSERTED FROM
DOCUMENTATION, because no selection-first editor was pinned. Those observables decide
D-EDITLANG-PRIMITIVE, which blocks C-EDITLANG and C-INPUT, and F-003 ships in the `usable-vim-tui`
MVP phase. The kernel primitive gets chosen either way; pinning Helix decides whether it is chosen
on observed or asserted evidence.

So: a count from this file is EVIDENCE, never a coverage ratio. ruse's `input.profile` enum is
[vim, emacs, native] — there is no helix profile — and FAM-EDIT-SELECTION carries `prd: []`.
Reading these numbers as parity would invent a compatibility promise no Decision has made.

WHY A PARSE AND NOT A PROBE
---------------------------
Three upstreams, three methods, and that is not inconsistency — it is upstreams.yaml's own rule
(source of record is per item type, empirically) applied to editors that answer to different
sources. Neovim publishes tables in-tree (parse). Emacs publishes nothing and exists only in a
running image (probe). Helix publishes neither: its surfaces are Rust literals — a `keymap!` macro
DSL, a `static_commands!` list, a `&[TypableCommand]` slice, a `Config` struct. So S is the source
of record for every surface here, and `hx` need not be installed.

The cost of that is stated rather than hidden: a static parse enumerates what EXISTS and cannot
observe BEHAVIOUR — and behaviour is the entire reason this upstream is pinned. This census does
not satisfy the three observables. It tells you which keys and commands exist to point an oracle
at; the oracle (`hx` in a pty) is a separate, still-open hole.

WHAT THE PARSE FOUND THAT THE DOCS DO NOT SAY
---------------------------------------------
Select mode is `normal.clone()` followed by `merge_nodes(overrides)`. Helix's Select is therefore
NOT a disjoint namespace the way Vim's Visual is — it is Normal with a diff, so every Normal
binding is reachable in Select unless explicitly overridden.

Be precise about what that is: a third CONSTRUCTION model, not a third dispatch model. The merge
happens before any key is pressed, so at dispatch each Helix mode is one flat map — Vim's depth-1
case. In the layered kernel model (CONCEPT-KEYMAP-DISPATCH) it is simply a depth-2 stack,
[override, base], collapsed early. That is the useful part: a layered router gets derived modes for
free, while a disjoint router must duplicate 301 bindings or grow a bespoke inheritance feature.

The extractor emits the override block AND the computed effective map so the diff stays visible
instead of being flattened away.

Regenerate:  python3 tools/parity/fetch.py helix && python3 tools/parity/extract_helix.py
"""
from __future__ import annotations

import argparse
import os
import re
import sys

import yaml

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))

from rusekit import render, repo  # noqa: E402

EDITOR = "helix"
OUT_DIR = "spec/parity/inventory/helix"
UPSTREAMS = "spec/parity/upstreams.yaml"
CACHE = ".ruse/cache/parity/helix"

HUMAN_FIELDS = ("status", "ruse", "note", "superseded_by", "surface_locked", "family", "secondary")

KEYMAP_RS = "helix-term/src/keymap/default.rs"
COMMANDS_RS = "helix-term/src/commands.rs"
TYPED_RS = "helix-term/src/commands/typed.rs"
EDITOR_RS = "helix-view/src/editor.rs"

# `"h" | "left" => move_char_left,` and `"g" => { "Goto"`
ENTRY = re.compile(r'^(?P<keys>"(?:[^"]*)"(?:\s*\|\s*"(?:[^"]*)")*)\s*=>\s*(?P<rest>.+)$')
LINE_COMMENT = re.compile(r"//.*$")
# `        move_char_left, "Move left",`
STATIC_CMD = re.compile(r'^\s{8}([a-z_0-9]+),\s*"((?:[^"\\]|\\.)*)"\s*,?\s*$')
# Config struct fields carry their default in the doc comment, not in the type.
FIELD = re.compile(r"^\s{4}pub ([a-z_0-9]+):\s*(.+?),?\s*$")


def _u(path: str) -> str:
    return os.path.join(repo.path(CACHE), path)


def _read(path: str) -> list[str]:
    with open(_u(path), encoding="utf-8") as fh:
        return fh.read().splitlines()


def _slug(s: str) -> str:
    s = s.replace("|", "-bar-").replace("\\", "-esc-")
    s = re.sub(r"[^A-Za-z0-9]+", "-", s).strip("-").lower()
    return s or "x"


def _dedupe(items: list[dict]) -> list[dict]:
    seen: dict[str, int] = {}
    for it in items:
        n = seen.get(it["id"], 0)
        seen[it["id"]] = n + 1
        if n:
            it["id"] = f"{it['id']}~{n + 1}"
    return items


# --------------------------------------------------------------------------- keymap DSL

def _block_bounds(lines: list[str], opener: str,
                  delims: tuple[str, str] = ("{", "}")) -> tuple[int, int, str]:
    """Locate one macro block by DELIMITER counting, not by indentation.

    Indentation would be the obvious rule and it is wrong here: the DSL nests, and a nested block's
    closing `},` sits at the same depth as sibling entries. Counting is the only reading that
    survives someone reformatting the file — and a census that silently truncates when upstream is
    reformatted is worse than one that fails.

    The delimiter pair is a parameter because the two macros disagree: `keymap!({ ... })` nests with
    braces, `static_commands!( ... )` with parentheses. Assuming braces for both is how the command
    surface first came back as 0 items — caught only because an empty surface is a hard failure.
    """
    op, cl = delims
    for i, line in enumerate(lines):
        if opener in line:
            label = re.search(r'"([^"]*)"', line)
            depth, j = 0, i
            while j < len(lines):
                depth += lines[j].count(op) - lines[j].count(cl)
                if depth <= 0 and j > i:
                    return i, j, (label.group(1) if label else "")
                j += 1
    raise SystemExit(f"{KEYMAP_RS}: block `{opener}` not found — upstream restructured the keymap "
                     f"DSL; the extractor must be re-read against the tree, not patched blindly")


def _parse_block(lines: list[str], i: int, end: int, prefix: list[str],
                 out: list[dict], mode: str, label: str) -> int:
    while i < end:
        raw = LINE_COMMENT.sub("", lines[i]).strip()
        i += 1
        if not raw:
            continue
        if raw.startswith("}"):
            return i
        m = ENTRY.match(raw)
        if not m:
            continue
        keys = re.findall(r'"([^"]*)"', m.group("keys"))
        rest = m.group("rest").strip()
        if rest.startswith("{"):
            sub = re.search(r'"([^"]*)"', rest)
            sublabel = sub.group(1) if sub else ""
            for k in keys:
                out.append({"mode": mode, "seq": prefix + [k], "binds": None,
                            "kind": "prefix", "group": sublabel, "block": label})
            i = _parse_block(lines, i, end, prefix + [keys[0]], out, mode, label)
        else:
            cmd = rest.rstrip(",").strip()
            for k in keys:
                out.append({"mode": mode, "seq": prefix + [k], "binds": cmd,
                            "kind": "command", "group": label, "block": label,
                            "aliases": [x for x in keys if x != k] or None})
    return i


def parse_keymap(lines: list[str], opener: str, mode: str) -> list[dict]:
    i, end, label = _block_bounds(lines, opener)
    out: list[dict] = []
    _parse_block(lines, i + 1, end, [], out, mode, label)
    if not out:
        raise SystemExit(f"{KEYMAP_RS}: block `{opener}` parsed to 0 bindings — the DSL changed "
                         f"shape; an empty surface is never accepted (gov parity_discovery)")
    return out


# --------------------------------------------------------------------------- surfaces

def extract_bindings() -> tuple[list[dict], list[dict], dict]:
    lines = _read(KEYMAP_RS)
    normal = parse_keymap(lines, 'keymap!({ "Normal mode"', "normal")
    override = parse_keymap(lines, 'keymap!({ "Select mode"', "select")
    insert = parse_keymap(lines, 'keymap!({ "Insert mode"', "insert")

    # `let mut select = normal.clone(); select.merge_nodes(...)` — Select is Normal PLUS a diff, not
    # a namespace of its own. Verified in-tree rather than assumed, because the whole finding rests
    # on it: if upstream ever builds Select independently, this census must stop reporting a derived
    # mode, and a silently wrong `derives_from` would be invisible in the item counts.
    src = "\n".join(lines)
    derived = ("let mut select = normal.clone();" in src
               and "select.merge_nodes(keymap!" in src)
    if not derived:
        raise SystemExit(f"{KEYMAP_RS}: Select is no longer built as `normal.clone()` + "
                         f"`merge_nodes` — re-read the file; the derived-mode model is a FINDING "
                         f"this census reports and it must not be asserted from a stale reading")

    over_seqs = {tuple(b["seq"]) for b in override}
    effective = [b for b in normal if tuple(b["seq"]) not in over_seqs] + override

    items, eff_items = [], []
    for b in normal + override + insert:
        seq = " ".join(b["seq"])
        it = {
            "id": f"helix.key.{b['mode']}.{_slug(seq)}",
            "surface": "key_binding",
            "mode": b["mode"],
            "key": seq,
            "kind": b["kind"],
            "binds": b["binds"] or "",
            "group": b["group"],
            "attestation": ["S"],
        }
        if b.get("aliases"):
            it["aliases"] = b["aliases"]
        if b["mode"] == "select":
            # The select block is the OVERRIDE, not the mode. Saying so on every item keeps a later
            # reader from counting 60-odd bindings and concluding Select is a small namespace.
            it["overrides_mode"] = "normal"
        items.append(it)

    for b in effective:
        seq = " ".join(b["seq"])
        eff_items.append({
            "id": f"helix.effkey.select.{_slug(seq)}",
            "surface": "effective_key_binding",
            "mode": "select",
            "key": seq,
            "kind": b["kind"],
            "binds": b["binds"] or "",
            "inherited": tuple(b["seq"]) not in over_seqs,
            "attestation": ["S"],
        })

    stats = {"normal": len(normal), "override": len(override), "insert": len(insert),
             "effective_select": len(effective),
             "inherited": sum(1 for e in eff_items if e["inherited"])}
    return _dedupe(items), _dedupe(eff_items), stats


def extract_modes(stats: dict) -> list[dict]:
    """The counterpart of nvim `map_mode` (8 disjoint) and emacs `keymap_tier` (9 layered).

    Three items, and the comparison is the point: Helix has a THIRD dispatch model — derived, where
    one mode is another plus a diff. Recording it as a surface rather than a note means
    gov parity_discovery locks it whole when anyone classifies it.
    """
    return [
        {"id": "helix.mode.normal", "surface": "mode", "mode": "Normal",
         "dispatch": "root", "derives_from": None,
         "bindings": stats["normal"],
         "unmatched_key": "ignore",
         "desc": "The root keymap; motions move and collapse the selection to a cursor.",
         "attestation": ["S"]},
        {"id": "helix.mode.select", "surface": "mode", "mode": "Select",
         "dispatch": "derived", "derives_from": "Normal",
         "bindings": stats["effective_select"],
         "override_bindings": stats["override"],
         "inherited_bindings": stats["inherited"],
         "unmatched_key": "ignore",
         "desc": ("Built as normal.clone() + merge_nodes(overrides): every Normal binding is "
                  "reachable unless overridden, and the merge is BUILD-TIME so dispatch stays flat "
                  "(a depth-2 layer stack collapsed early). Motions EXTEND rather than move — the "
                  "OBS-BARE-MOTION divergence, structurally located."),
         "attestation": ["S"]},
        {"id": "helix.mode.insert", "surface": "mode", "mode": "Insert",
         "dispatch": "root", "derives_from": None,
         "bindings": stats["insert"],
         "unmatched_key": "insert",
         "desc": "Independent root keymap; unmatched printable keys insert literally.",
         "attestation": ["S"]},
    ]


def extract_static_commands() -> list[dict]:
    lines = _read(COMMANDS_RS)
    i, end, _ = _block_bounds(lines, "static_commands!(", ("(", ")"))
    out = []
    for line in lines[i + 1:end]:
        m = STATIC_CMD.match(LINE_COMMENT.sub("", line))
        if m:
            out.append({
                "id": f"helix.command.{_slug(m.group(1))}",
                "surface": "command",
                "name": m.group(1),
                "desc": m.group(2),
                "attestation": ["S"],
            })
    return _dedupe(out)


def extract_typed_commands() -> list[dict]:
    src = "\n".join(_read(TYPED_RS))
    start = src.index("pub const TYPABLE_COMMAND_LIST")
    body = src[start:]
    # Chunk-then-parse rather than one regex over the whole list. A single pattern threading
    # name -> aliases -> doc silently dropped `line-ending`, whose entry carries `#[cfg(...)]`
    # attributes and TWO doc strings selected by the `unicode-lines` feature. One missing item out
    # of 89 is exactly the kind of loss a census must not absorb quietly, so the count is asserted
    # below instead of trusted.
    chunks = body.split("TypableCommand {")[1:]
    out = []
    for chunk in chunks:
        nm = re.search(r'^\s*name:\s*"([^"]*)"', chunk)
        if not nm:
            continue
        al = re.search(r'aliases:\s*&\[([^\]]*)\]', chunk)
        docs = re.findall(r'doc:\s*"((?:[^"\\]|\\.)*)"', chunk)
        item = {
            "id": f"helix.typed.{_slug(nm.group(1))}",
            "surface": "typed_command",
            "name": nm.group(1),
            "aliases": re.findall(r'"([^"]*)"', al.group(1)) if al else [],
            "desc": (docs[0] if docs else "").replace("\\n", " ").strip(),
            "attestation": ["S"],
        }
        if len(docs) > 1:
            # Upstream's own surface is build-configuration dependent; flattening that to one doc
            # would make the census claim a certainty the source does not have.
            item["cfg_dependent"] = True
            item["desc_variants"] = [d.replace("\\n", " ").strip() for d in docs]
        out.append(item)
    declared = len(chunks)
    if len(out) != declared:
        raise SystemExit(f"{TYPED_RS}: {declared} TypableCommand blocks but {len(out)} parsed — "
                         f"a silent shortfall in the denominator; re-read the file")
    return _dedupe(out)


def extract_options() -> tuple[list[dict], dict]:
    """Config fields, dotted through nested config structs.

    Flattening only the top-level `Config` would report ~51 options against Neovim's 374 and invite
    the conclusion that Helix is 7x simpler. It is not — its config NESTS (`editor.lsp.*`,
    `editor.statusline.*`), so the comparable number requires walking into the child structs. The
    walk is depth-bounded and every unresolved field is reported rather than dropped.
    """
    lines = _read(EDITOR_RS)
    structs: dict[str, list[tuple[str, str, str]]] = {}
    name = None
    doc: list[str] = []
    for line in lines:
        st = re.match(r"^pub struct ([A-Za-z0-9]+) \{", line)
        if st:
            name, doc = st.group(1), []
            structs[name] = []
            continue
        if name is None:
            continue
        if line.startswith("}"):
            name = None
            continue
        d = re.match(r"^\s*///\s?(.*)$", line)
        if d:
            doc.append(d.group(1).strip())
            continue
        f = FIELD.match(line)
        if f:
            structs[name].append((f.group(1), f.group(2), " ".join(doc)))
            doc = []
        elif line.strip() and not line.strip().startswith("#["):
            doc = []

    if "Config" not in structs:
        raise SystemExit(f"{EDITOR_RS}: `pub struct Config` not found — upstream moved the config "
                         f"root; the option denominator cannot be guessed")

    out: list[dict] = []
    unresolved: list[str] = []

    def walk(struct: str, prefix: str, depth: int) -> None:
        for field, ty, desc in structs.get(struct, []):
            dotted = f"{prefix}{field}"
            inner = re.sub(r"^(Option|Vec|Box)<(.+)>$", r"\2", ty.strip())
            if inner in structs and inner != struct and depth < 4:
                out.append({"id": f"helix.option.{_slug(dotted)}", "surface": "option",
                            "name": dotted, "type": ty, "kind": "section",
                            "desc": desc, "attestation": ["S"]})
                walk(inner, f"{dotted}.", depth + 1)
            else:
                out.append({"id": f"helix.option.{_slug(dotted)}", "surface": "option",
                            "name": dotted, "type": ty, "kind": "leaf",
                            "desc": desc, "attestation": ["S"]})
                if ty.strip() not in ("bool", "usize", "isize", "char", "String", "u64", "f64") \
                        and not ty.startswith(("Vec<", "Option<", "HashMap<")):
                    unresolved.append(f"{dotted}: {ty}")

    walk("Config", "", 0)
    return _dedupe(out), {"structs": len(structs), "unresolved": unresolved}


# --------------------------------------------------------------------------- emit

def load_existing(fname: str) -> dict[str, dict]:
    path = repo.path(OUT_DIR, fname)
    if not os.path.exists(path):
        return {}
    with open(path, encoding="utf-8") as fh:
        doc = yaml.safe_load(fh) or {}
    return {i["id"]: i for i in (doc.get("items") or []) if "id" in i}


def merge(fresh: list[dict], prior: dict[str, dict]) -> tuple[list[dict], int, int]:
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


def write_surface(fname: str, title: str, source: str, items: list[dict],
                  meta: dict, dry: bool) -> dict:
    prior = load_existing(fname)
    merged, added, gone = merge(items, prior)
    stats = {"total": len(merged), "added": added, "upstream_gone": gone,
             "unclassified": sum(1 for i in merged if i.get("status") == "unclassified")}
    if dry:
        return stats
    doc = {
        "version": 1,
        "generated": True,
        "generator": "tools/parity/extract_helix.py",
        "upstream": EDITOR,
        "revision": meta["revision"],
        "version_label": meta["version_label"],
        # Not a parity target. Repeated per file because a generated inventory is read on its own,
        # far from upstreams.yaml, and a count without this line reads as coverage.
        "role": "reference",
        "not_a_parity_target": ("Pinned as EVIDENCE for concepts/irreconcilable.yaml#observables "
                                "(D-EDITLANG-PRIMITIVE). ruse declares no helix input profile; "
                                "these counts are never a coverage ratio."),
        "surface": title,
        "source_of_record": source,
        "items": merged,
    }
    os.makedirs(repo.path(OUT_DIR), exist_ok=True)
    header = (
        "# GENERATED — do not hand-edit item enumeration. Regenerate:\n"
        "#   python3 tools/parity/fetch.py helix && python3 tools/parity/extract_helix.py\n"
        f"# Human-owned fields ({', '.join(HUMAN_FIELDS)}) ARE preserved across regeneration; the\n"
        "# enumeration is not. `status: unclassified` is legitimate and expected — discovery is\n"
        "# strict, classification is lazy (and locked per surface by `ruse gov parity_discovery`).\n"
        "# role: reference — see `not_a_parity_target` below before quoting any number from here.\n"
    )
    with open(repo.path(OUT_DIR, fname), "w", encoding="utf-8") as fh:
        fh.write(header)
        yaml.safe_dump(doc, fh, sort_keys=False, allow_unicode=True, width=120)
    return stats


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(prog="parity extract_helix")
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args(argv if argv is not None else sys.argv[1:])

    with open(repo.path(UPSTREAMS), encoding="utf-8") as fh:
        cfg = ((yaml.safe_load(fh) or {}).get("upstreams") or {}).get(EDITOR) or {}
    if not os.path.isdir(_u("helix-term")):
        render.fail(f"upstream cache missing — run: python3 tools/parity/fetch.py {EDITOR}")
        return 1

    meta = {"revision": cfg.get("revision", "?"), "version_label": cfg.get("version_label", "?")}
    render.heading(f"parity extract — {EDITOR} @ {meta['version_label']} "
                   f"({str(meta['revision'])[:12]}) · role: reference, NOT a parity target")

    bindings, effective, kstats = extract_bindings()
    options, ostats = extract_options()
    surfaces = [
        ("mode.yaml", "mode", f"S: {KEYMAP_RS}", extract_modes(kstats)),
        ("key_binding.yaml", "key_binding", f"S: {KEYMAP_RS}", bindings),
        ("effective_key_binding.yaml", "effective_key_binding",
         f"S: {KEYMAP_RS} (COMPUTED: normal + select overrides)", effective),
        ("command.yaml", "command", f"S: {COMMANDS_RS} static_commands!",
         extract_static_commands()),
        ("typed_command.yaml", "typed_command", f"S: {TYPED_RS} TYPABLE_COMMAND_LIST",
         extract_typed_commands()),
        ("option.yaml", "option", f"S: {EDITOR_RS} Config (dotted through nested structs)", options),
    ]

    total = 0
    for fname, title, source, items in surfaces:
        st = write_surface(fname, title, source, items, meta, args.dry_run)
        total += st["total"]
        gone = f", {st['upstream_gone']} upstream-gone" if st["upstream_gone"] else ""
        render.bullet(f"{title:<22} {st['total']:>5} items ({st['unclassified']} unclassified{gone})")

    render.field("Findings", "")
    render.bullet(f"Select is DERIVED, not disjoint: normal.clone() + merge_nodes -> "
                  f"{kstats['effective_select']} effective bindings, of which "
                  f"{kstats['inherited']} are inherited from Normal and {kstats['override']} "
                  f"override it. Vim's Visual is a separate namespace. At DISPATCH this is still "
                  f"flat (depth-1, like Vim); the derivation is build-time — a depth-2 layer stack "
                  f"collapsed early, which a layered router expresses for free")
    render.bullet(f"modes: 3 (helix, flat+derived) vs map_mode 8 (nvim, disjoint) vs keymap_tier 9 "
                  f"(emacs, layered) — compare the MODELS, not the counts; all three are cases of "
                  f"one ordered layer stack (CONCEPT-KEYMAP-DISPATCH)")
    render.bullet(f"options walked through {ostats['structs']} structs; "
                  f"{len(ostats['unresolved'])} field(s) have a type the walk could not resolve "
                  f"(reported, not dropped)")
    for u in ostats["unresolved"][:8]:
        render.bullet(f"  unresolved type: {u}")
    render.ok(f"{total} upstream items enumerated"
              + (" (dry run — nothing written)" if args.dry_run else f" → {OUT_DIR}/"))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
