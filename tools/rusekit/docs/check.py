"""docs-check — documentation hygiene that spec-validate does not cover.

spec-validate already checks that the *file* part of every relative link resolves. This adds
the delta, biased (like spec-validate) toward ZERO false positives:

  * same-file `#anchor` links must resolve to a heading  -> ERROR (slug is computed reliably)
  * cross-file `file.md#anchor` links                    -> WARN (slug algorithms vary)
  * docs/ prose files missing YAML frontmatter           -> WARN
  * normative words (MUST/SHALL/REQUIRED/...) added to plain docs prose -> WARN
    (a real contract belongs in spec/, not in an explanation — ENG-DOC-001)
  * an `apps/…` / `crates/…` source path that names no file or directory -> WARN
    (a split moved the code). Scanned in docs/ and spec/ Markdown AND in spec/**/*.yaml, where
    `evidence:`/`source:` lists cite code. Dated records quote the tree as it was and are
    skipped: docs/reviews/, accepted/rejected RFCs, and the spec/DECISIONS.md log.
  * docs/contributing/module-layout.md re-measured (rusekit.docs.module_layout, #1689): a structural
    mismatch (the L2 `x/mod.rs` set, the T3 directory list, the hub set) -> ERROR; a stale volatile
    tally -> WARN. `--write` rewrites the stale numbers.
  * a `path.rs::name` evidence reference whose file exists but defines no `name` -> WARN.
    Rule: only the FIRST `::` segment is checked, and it must appear in the file as
    `fn|struct|enum|trait|type|const|static|mod|union|macro_rules! <name>` (a plain grep, no
    parsing), so `file.rs::Type::method` checks `Type`. A non-`.rs` path is not checked.

Only same-file broken anchors and module-layout structure mismatches fail the command; the rest is advisory.
"""
from __future__ import annotations

import argparse
import glob
import os
import re
import sys

from rusekit import repo, render, contract  # noqa: E402
from rusekit.docs import module_layout  # noqa: E402

# Subtrees where normative words are legitimate (specs of contracts, not explanations).
LEAK_SKIP = ("docs/rfc/", "docs/invariants/", "docs/anti-patterns/", "docs/protocols/")
LINK_RE = re.compile(r"\]\(([^)]+)\)")
HEADING_RE = re.compile(r"^#{1,6}\s+(.*\S)\s*$")
# Explicit heading id: `### Title {#custom-id}` (pandoc/mkdocs style, used across docs/).
EXPLICIT_ID_RE = re.compile(r"\{#([\w-]+)\}\s*$")
# A source path in prose: `apps/…` or `crates/…`. It stops at `:` (a `:line` or `::item` suffix).
SRC_PATH_RE = re.compile(r"(?<![\w/.-])((?:apps|crates)/[\w./-]*\w)")
# The `::name` evidence suffix right after a path (only its first segment is captured).
ITEM_SUFFIX_RE = re.compile(r"::([A-Za-z_]\w*)")
# Dated records quote the tree as it was; a path there is history, not a pointer.
SRC_PATH_SKIP = ("docs/reviews/", "docs/rfc/accepted/", "docs/rfc/rejected/", "spec/DECISIONS.md")
# Explicit exemptions, (file, reference) -> why. Only for a reference that is deliberately not a
# live pointer (a quote of the tree as it was, a placeholder example) or whose code was deleted
# rather than moved and awaits a follow-up decision. A moved path is fixed, never listed here.
SRC_REF_ALLOW: dict[tuple[str, str], str] = {
    ("spec/parity/concepts/irreconcilable.yaml", "crates/core/src/editor.rs"):
        "history: 'the Mode enum then in crates/core/src/editor.rs' quotes the pre-split tree",
    ("spec/parity/contracts/keymap-layers.yaml", "apps/tui/src/input.rs"):
        "history: the text already says '(now `input/mod.rs`)'",
    ("spec/waivers.yaml", "crates/workspace-runtime"):
        "placeholder: a commented-out example waiver, not a crate of this repo",
}
_ITEM_KINDS = r"(?:fn|struct|enum|trait|type|const|static|mod|union|macro_rules!)"


def slugify(text: str) -> str:
    # GitHub's slugger: lowercase, strip punctuation (keeping word chars, whitespace,
    # hyphens), then map each whitespace char to ONE hyphen — consecutive spaces are NOT
    # collapsed (so "TITLE — Sub" → "title--sub", matching a removed space-flanked em-dash).
    s = text.strip().lower()
    s = re.sub(r"[^\w\s-]", "", s)
    s = re.sub(r"\s", "-", s)
    return s


def headings_slugs(text: str) -> set[str]:
    counts: dict[str, int] = {}
    slugs: set[str] = set()
    in_fence = False
    for line in text.splitlines():
        if line.lstrip().startswith("```"):
            in_fence = not in_fence
            continue
        if in_fence:
            continue
        m = HEADING_RE.match(line)
        if m:
            title = m.group(1)
            explicit = EXPLICIT_ID_RE.search(title)
            if explicit:
                slugs.add(explicit.group(1))          # explicit id wins verbatim
                title = EXPLICIT_ID_RE.sub("", title)  # ...and GitHub also auto-slugs the text
            base = slugify(title)
            n = counts.get(base, 0)
            slugs.add(base if n == 0 else f"{base}-{n}")
            counts[base] = n + 1
    return slugs


def defines_item(source: str, name: str) -> bool:
    """True when `source` declares `name` as a Rust item (the grep rule in the module doc)."""
    return re.search(rf"\b{_ITEM_KINDS}\s+{re.escape(name)}\b", source) is not None


def stale_source_paths(text: str, exists, has_item=None) -> list[tuple[int, str]]:
    """(lineno, ref) for each `apps/…`/`crates/…` path in `text` that `exists` rejects. A path
    followed by `*`, `{` or `<` is a glob, a set or a placeholder, not a path, and is skipped.
    With `has_item(path, name)`, a live `.rs` path followed by `::name` is also reported, as
    `path::name`, when `has_item` rejects the name."""
    out: list[tuple[int, str]] = []
    for lineno, line in enumerate(text.splitlines(), 1):
        for m in SRC_PATH_RE.finditer(line):
            path, rest = m.group(1), line[m.end():]
            if rest.lstrip("/")[:1] in ("*", "{", "<", "\\"):
                continue
            if not exists(path):
                out.append((lineno, path))
                continue
            item = ITEM_SUFFIX_RE.match(rest)
            if has_item and item and path.endswith(".rs") and not has_item(path, item.group(1)):
                out.append((lineno, f"{path}::{item.group(1)}"))
    return out


def _repo_has_item():
    cache: dict[str, str] = {}

    def has_item(path: str, name: str) -> bool:
        p = repo.path(path)
        if p not in cache:
            cache[p] = open(p, encoding="utf-8").read() if os.path.isfile(p) else ""
        return defines_item(cache[p], name)
    return has_item


def _prose_lines(text: str):
    """Yield (lineno, line) for lines outside fenced code blocks."""
    in_fence = False
    for i, line in enumerate(text.splitlines(), 1):
        if line.lstrip().startswith("```"):
            in_fence = not in_fence
            continue
        if in_fence:
            continue
        yield i, line


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(prog="ruse docs check")
    ap.add_argument("--max-warn", type=int, default=40, help="cap warnings printed")
    ap.add_argument("--write", action="store_true",
                    help=f"rewrite stale counts in {module_layout.DOC} before checking")
    args = ap.parse_args(argv)

    marker = contract.load_kinds().get("generated_marker")
    leak_terms = contract.load_kinds().get("normative_leak_terms") or []
    leak_re = re.compile(r"\b(" + "|".join(re.escape(t) for t in leak_terms) + r")\b") \
        if leak_terms else None

    files = sorted(glob.glob(repo.path("docs/**/*.md"), recursive=True)
                   + glob.glob(repo.path("spec/**/*.md"), recursive=True))
    spec_yaml = sorted(glob.glob(repo.path("spec/**/*.yaml"), recursive=True))

    errors, warns = module_layout.run(repo.ROOT, write=args.write)
    generated: list[str] = []
    slug_cache: dict[str, set[str]] = {}

    def slugs_of(relfile: str) -> set[str]:
        if relfile not in slug_cache:
            p = repo.path(relfile)
            slug_cache[relfile] = headings_slugs(open(p, encoding="utf-8").read()) \
                if os.path.isfile(p) else set()
        return slug_cache[relfile]

    exists = lambda q: os.path.exists(repo.path(q))  # noqa: E731
    has_item = _repo_has_item()
    allow_used: set[tuple[str, str]] = set()

    def source_ref_warns(rel: str, text: str) -> None:
        if rel.startswith(SRC_PATH_SKIP):
            return
        for lineno, ref in stale_source_paths(text, exists, has_item):
            if (rel, ref) in SRC_REF_ALLOW:
                allow_used.add((rel, ref))
                continue
            what = "names no item" if "::" in ref else "does not exist"
            warns.append(f"{rel}:{lineno}: source reference '{ref}' {what}")

    for p in spec_yaml:
        source_ref_warns(repo.rel(p), open(p, encoding="utf-8").read())

    for p in files:
        rel = repo.rel(p)
        text = open(p, encoding="utf-8").read()
        if marker and marker in text[:2048]:
            generated.append(rel)
            continue  # derived file — don't lint

        own = headings_slugs(text)
        slug_cache[rel] = own
        base = os.path.dirname(rel)

        # frontmatter (docs/ prose only; skip README and spec/*.md registries)
        if rel.startswith("docs/") and os.path.basename(rel) != "README.md":
            if not text.lstrip().startswith("---"):
                warns.append(f"{rel}: missing YAML frontmatter")

        # anchors
        for m in LINK_RE.finditer(text):
            href = m.group(1).strip()
            if href.startswith(("http", "mailto:", "//")) or "#" not in href:
                continue
            filepart, _, anchor = href.partition("#")
            if not anchor:
                continue
            if not filepart:  # same-file anchor — reliable
                if slugify(anchor) not in own and own:
                    errors.append(f"{rel}: broken same-file anchor '#{anchor}'")
            elif filepart.endswith(".md"):
                target = os.path.normpath(os.path.join(base, filepart)).replace(os.sep, "/")
                tslugs = slugs_of(target)
                if tslugs and slugify(anchor) not in tslugs:
                    warns.append(f"{rel}: anchor '{filepart}#{anchor}' not found in target")

        # source paths (advisory)
        source_ref_warns(rel, text)

        # normative leak (advisory)
        if leak_re and rel.startswith("docs/") and not rel.startswith(LEAK_SKIP):
            for lineno, line in _prose_lines(text):
                # ignore inline code spans
                stripped = re.sub(r"`[^`]*`", "", line)
                m = leak_re.search(stripped)
                if m:
                    warns.append(f"{rel}:{lineno}: normative '{m.group(1)}' in docs prose "
                                 f"(belongs in spec/ if it is a contract)")

    for rel, ref in sorted(set(SRC_REF_ALLOW) - allow_used):
        warns.append(f"{rel}: allowlisted source reference '{ref}' no longer occurs — drop it "
                     f"from SRC_REF_ALLOW")

    render.heading("docs-check")
    render.field("Files scanned", f"{len(files)} Markdown, {len(spec_yaml)} spec YAML")
    render.field("Generated (skipped)", str(len(generated)))
    render.field("Errors", str(len(errors)))
    render.field("Warnings", str(len(warns)))
    if errors:
        render.heading("\nErrors")
        for e in errors:
            render.bullet(e, mark="✗")
    if warns:
        render.heading("\nWarnings")
        for w in warns[:args.max_warn]:
            render.warn(w)
        if len(warns) > args.max_warn:
            print(f"      … and {len(warns) - args.max_warn} more")
    print()
    if errors:
        render.fail(f"docs-check: FAIL ({len(errors)} errors)")
        return 1
    render.ok(f"docs-check: PASS" + (f" ({len(warns)} warnings)" if warns else ""))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
