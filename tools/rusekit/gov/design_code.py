#!/usr/bin/env python3
"""gov design_code — enforce that code in design docs is NON-NORMATIVE, and diff it against real code (D-038).

Governance policy (D-038): a design doc's job is the CONTRACT — invariants, field semantics, algorithms —
not the concrete type. Concrete types have one authoritative home: code (internal types), or spec/contracts/
(cross-boundary formats/protocols). A hand-written struct in prose is a drift liability: when the code or
design changes, nobody chases the copies.

Two checks, both portable + auto-discovered into `ruse gov check`:

1. BANNER (warn-only): any design/RFC doc that shows code must mark it `code-blocks: illustrative`, so a
   sketch never reads as authoritative.

2. DOC<->CODE DIFF (this file's successor check, now that code exists): extract each `struct`/`enum` shown in
   a doc's illustrative Rust and, for any type that is ALSO implemented for real in crates/apps, compare the
   member (field/variant) NAMES, and SURFACE any divergence so a human reconciles it.

   IMPORTANT — the tool does NOT decide which side is authoritative. A doc-only member can mean the design
   is deliberately AHEAD of the code (the code is the thing to fix), OR that the illustration is stale (the
   doc is the thing to fix). D-038 makes code the SSOT for a concrete TYPE, but the DESIGN CONTRACT can lead
   the implementation in this spec-first project. So this check reports the divergence neutrally; it never
   prescribes "trim the doc." The owner records the resolution — including an intentional, standing
   divergence — with an in-doc `<!-- design-code-ack: TypeName — reason -->`.

   To stay low-noise the diff is deliberately conservative:
     - only types present in BOTH a doc and real code are compared (unimplemented design types are ignored);
     - a doc/code pair is only treated as the same type when their members OVERLAP (or both are tuple/unit) —
       this rejects unrelated name collisions (e.g. a register `Slot` vs an anchor `Slot`);
     - a divergence is reported when the doc and code member sets differ by a doc-only member or a KIND
       mismatch; a code superset of the doc (illustration is a strict simplification) is treated as in-sync;
     - a type named in a `design-code-ack` comment is acknowledged (owned) and not reported.
   Warn-only by default; `--strict` makes an UNACKNOWLEDGED divergence a failure (a blocking gate later).
"""
from __future__ import annotations

import argparse
import glob
import os
import re
import sys

from rusekit import repo, render  # noqa: E402


def _read(path: str) -> str:
    with open(path, encoding="utf-8") as fh:
        return fh.read()

# Language-tagged fences that are actual code (not ascii diagrams / shell transcripts we don't govern).
CODE_FENCE = re.compile(r"^```(rust|python|toml|json|ts|typescript|javascript)\b", re.M)
RUST_FENCE = re.compile(r"```rust\b(.*?)```", re.S)
BANNER = "code-blocks: illustrative"   # the marker a design doc adds once (near its code or in frontmatter)
# An owned, in-doc acknowledgement that a doc<->code divergence is KNOWN and intentional — in EITHER
# direction (the design is deliberately ahead of the code, OR the illustration is a simplification the
# owner accepts). The tool does not decide which side is authoritative; the owner records that and why.
# Format: `<!-- design-code-ack: TypeA, TypeB — reason -->`.
ACK = re.compile(r"design-code-ack:\s*([A-Za-z0-9_,\s]+)")
DOC_ROOTS = ("docs/design", "docs/rfc")
CODE_ROOTS = ("crates", "apps")


# ----------------------------------------------------------------------------------------------------------
# Lightweight Rust type parsing (stdlib only — no `syn` dependency, D-034). Extracts struct/enum member NAMES;
# it does not need to be a full parser, only to agree on the set of field/variant identifiers.
# ----------------------------------------------------------------------------------------------------------

def _strip_noise(text: str) -> str:
    text = re.sub(r"/\*.*?\*/", "", text, flags=re.S)   # block comments
    text = re.sub(r"//.*", "", text)                    # line comments (incl. /// doc comments)
    text = re.sub(r"#\[.*?\]", "", text, flags=re.S)    # attributes
    return text


def _split_top_level(body: str) -> list[str]:
    """Split a struct/enum body on commas that are not nested inside (), [], {} or <>."""
    segs, depth, cur = [], 0, ""
    for c in body:
        if c in "([{<":
            depth += 1
        elif c in ")]}>":
            depth -= 1
        if c == "," and depth == 0:
            segs.append(cur)
            cur = ""
        else:
            cur += c
    if cur.strip():
        segs.append(cur)
    return segs


def _body_after(text: str, start: int) -> str | None:
    """The brace body that opens after `start`, or None for a tuple/unit type (`(...)` / `;` first)."""
    angle, i, n = 0, start, len(text)
    while i < n:
        c = text[i]
        if c == "<":
            angle += 1
        elif c == ">":
            angle -= 1
        elif angle <= 0:
            if c == "{":
                depth, j = 0, i
                while j < n:
                    if text[j] == "{":
                        depth += 1
                    elif text[j] == "}":
                        depth -= 1
                        if depth == 0:
                            return text[i + 1:j]
                    j += 1
                return text[i + 1:]
            if c in "(;":
                return None
        i += 1
    return None


def parse_rust_types(text: str) -> dict[str, dict]:
    """Map `TypeName -> {kind, members}` for every struct/enum declared in `text`. `members` is the set of
    field names (struct) or variant names (enum); a tuple/unit type has an empty set."""
    text = _strip_noise(text)
    out: dict[str, dict] = {}
    for m in re.finditer(r"\b(struct|enum)\s+([A-Z][A-Za-z0-9_]*)", text):
        kind, name = m.group(1), m.group(2)
        body = _body_after(text, m.end())
        members: set[str] = set()
        if body is not None:
            for seg in _split_top_level(body):
                seg = seg.strip()
                if not seg:
                    continue
                if kind == "struct":
                    fm = re.match(r"(?:pub\s*(?:\([^)]*\)\s*)?)?([a-z_]\w*)\s*:", seg)
                    if fm:
                        members.add(fm.group(1))
                else:
                    vm = re.match(r"([A-Z]\w*)", seg)
                    if vm:
                        members.add(vm.group(1))
        entry = out.setdefault(name, {"kind": kind, "members": set()})
        entry["kind"] = kind
        entry["members"] |= members
    return out


# ----------------------------------------------------------------------------------------------------------
# Scans + the diff
# ----------------------------------------------------------------------------------------------------------

def scan_banner() -> list[dict]:
    out = []
    for root in DOC_ROOTS:
        for p in sorted(glob.glob(repo.path(root, "**", "*.md"), recursive=True)):
            text = _read(p)
            if not CODE_FENCE.search(text):
                continue
            out.append({"rel": repo.rel(p), "banner": BANNER in text,
                        "blocks": len(CODE_FENCE.findall(text))})
    return out


def scan_code_types() -> dict[str, dict]:
    code: dict[str, dict] = {}
    for root in CODE_ROOTS:
        for p in glob.glob(repo.path(root, "**", "*.rs"), recursive=True):
            if os.sep + "tests" + os.sep in p or p.endswith("_test.rs"):
                continue  # integration tests are not the design surface
            for name, info in parse_rust_types(_read(p)).items():
                e = code.setdefault(name, {"kind": info["kind"], "members": set()})
                e["members"] |= info["members"]
                e["kind"] = info["kind"]
    return code


def scan_doc_types() -> dict[str, dict]:
    doc: dict[str, dict] = {}
    for root in DOC_ROOTS:
        for p in sorted(glob.glob(repo.path(root, "**", "*.md"), recursive=True)):
            blocks = "\n".join(RUST_FENCE.findall(_read(p)))
            if not blocks:
                continue
            for name, info in parse_rust_types(blocks).items():
                e = doc.setdefault(name, {"kind": info["kind"], "members": set(), "docs": set()})
                e["members"] |= info["members"]
                e["kind"] = info["kind"]
                e["docs"].add(repo.rel(p))
    return doc


def scan_ack() -> set[str]:
    """Type names the docs explicitly acknowledge as a known, owned doc<->code divergence."""
    acked: set[str] = set()
    for root in DOC_ROOTS:
        for p in glob.glob(repo.path(root, "**", "*.md"), recursive=True):
            for m in ACK.findall(_read(p)):
                acked |= {t.strip() for t in m.split(",") if t.strip()}
    return acked


def diff_types(doc: dict[str, dict], code: dict[str, dict], acked: set[str] | None = None) -> list[dict]:
    """Divergence findings for types present in both, matched conservatively (see module docstring). The
    tool reports the divergence; it does not decide which side is authoritative. Acknowledged types are
    still returned but flagged `acked=True` so the caller can list-but-not-warn them."""
    acked = acked or set()
    findings = []
    for name in sorted(set(doc) & set(code)):
        d, c = doc[name], code[name]
        overlap = d["members"] & c["members"]
        both_opaque = not d["members"] and not c["members"]
        # Same type only if members overlap, or both are tuple/unit (e.g. `Revision(u64)`).
        if not overlap and not both_opaque:
            continue
        kind_mismatch = d["kind"] != c["kind"]
        doc_only = sorted(d["members"] - c["members"])
        # A divergence worth a human's eyes = the doc and code disagree on a member the doc names, or on the
        # struct/enum kind. A pure code-superset (illustration is a strict simplification) is in-sync.
        if kind_mismatch or doc_only:
            findings.append({
                "name": name,
                "docs": sorted(d["docs"]),
                "kind_doc": d["kind"],
                "kind_code": c["kind"],
                "kind_mismatch": kind_mismatch,
                "doc_only": doc_only,
                "code_only": sorted(c["members"] - d["members"]),
                "acked": name in acked,
            })
    return findings


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(prog="gov design_code")
    ap.add_argument("--strict", action="store_true",
                    help="exit non-zero on banner-missing or doc<->code divergence (blocking gate)")
    args = ap.parse_args(argv or [])

    banner_docs = scan_banner()
    missing = [d for d in banner_docs if not d["banner"]]

    code = scan_code_types()
    doc = scan_doc_types()
    acked_names = scan_ack()
    findings = diff_types(doc, code, acked_names)
    checked = sorted(set(doc) & set(code))
    active = [f for f in findings if not f["acked"]]
    acked = [f for f in findings if f["acked"]]

    render.heading("gov design_code (D-038: design-doc code is non-normative + doc<->code diff)")
    render.field("Design/RFC docs with code", str(len(banner_docs)))
    render.field("Missing illustrative banner", str(len(missing)))
    render.field("Types shared doc<->code", str(len(checked)))
    render.field("Diverging (unacknowledged)", str(len(active)))
    render.field("Diverging (acknowledged)", str(len(acked)))

    for d in missing:
        render.bullet(f"{d['rel']}  ({d['blocks']} code block(s)) — add `{BANNER}` banner", mark="!")
    for f in active:
        where = ", ".join(f["docs"])
        if f["kind_mismatch"]:
            render.bullet(f"{f['name']}: doc says `{f['kind_doc']}`, code says `{f['kind_code']}` [{where}]",
                          mark="!")
        # Neutral: the design may lead the code, or the illustration may be stale — a human decides which is
        # authoritative and either updates the code, trims the doc, or records a `design-code-ack`.
        detail = f"doc-only members {f['doc_only']}" if f["doc_only"] else "kind differs"
        if f["code_only"]:
            detail += f"; code-only {f['code_only']}"
        render.bullet(f"{f['name']}: doc illustration and code diverge ({detail}) — reconcile: is the design "
                      f"ahead of the code, or the illustration stale? [{where}]", mark="!")
    for f in acked:
        render.bullet(f"{f['name']}: divergence acknowledged (design-code-ack) — {', '.join(f['docs'])}",
                      mark="·")

    if not missing and not active:
        render.ok("design-doc code is illustrative; no unacknowledged doc<->code divergence (D-038)")
        return 0
    msg = (f"{len(missing)} banner-missing, {len(active)} unacknowledged diverging type(s) "
           "— reconcile (update code, trim the illustration, or add a design-code-ack)")
    if args.strict:
        render.fail(msg)
        return 1
    render.warn(msg + " (warn; use --strict to block)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
