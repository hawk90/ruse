"""docs-check — documentation hygiene that spec-validate does not cover.

spec-validate already checks that the *file* part of every relative link resolves. This adds
the delta, biased (like spec-validate) toward ZERO false positives:

  * same-file `#anchor` links must resolve to a heading  -> ERROR (slug is computed reliably)
  * cross-file `file.md#anchor` links                    -> WARN (slug algorithms vary)
  * docs/ prose files missing YAML frontmatter           -> WARN
  * normative words (MUST/SHALL/REQUIRED/...) added to plain docs prose -> WARN
    (a real contract belongs in spec/, not in an explanation — ENG-DOC-001)
  * an `apps/…` / `crates/…` source path that names no file or directory -> WARN
    (a split moved the code; dated reviews under docs/reviews/ are history and are skipped)

Only same-file broken anchors fail the command; the rest is advisory.
"""
from __future__ import annotations

import argparse
import glob
import os
import re
import sys

from rusekit import repo, render, contract  # noqa: E402

# Subtrees where normative words are legitimate (specs of contracts, not explanations).
LEAK_SKIP = ("docs/rfc/", "docs/invariants/", "docs/anti-patterns/", "docs/protocols/")
LINK_RE = re.compile(r"\]\(([^)]+)\)")
HEADING_RE = re.compile(r"^#{1,6}\s+(.*\S)\s*$")
# Explicit heading id: `### Title {#custom-id}` (pandoc/mkdocs style, used across docs/).
EXPLICIT_ID_RE = re.compile(r"\{#([\w-]+)\}\s*$")
# A source path in prose: `apps/…` or `crates/…`. It stops at `:` (a `:line` or `::item` suffix).
SRC_PATH_RE = re.compile(r"(?<![\w/.-])((?:apps|crates)/[\w./-]*\w)")
# Dated records quote the tree as it was; a path there is history, not a pointer.
SRC_PATH_SKIP = ("docs/reviews/",)


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


def stale_source_paths(text: str, exists) -> list[tuple[int, str]]:
    """(lineno, path) for each `apps/…`/`crates/…` path in `text` that `exists` rejects. A path
    followed by `*`, `{` or `<` is a glob, a set or a placeholder, not a path, and is skipped."""
    out: list[tuple[int, str]] = []
    for lineno, line in enumerate(text.splitlines(), 1):
        for m in SRC_PATH_RE.finditer(line):
            if line[m.end():].lstrip("/")[:1] in ("*", "{", "<"):
                continue
            if not exists(m.group(1)):
                out.append((lineno, m.group(1)))
    return out


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
    args = ap.parse_args(argv)

    marker = contract.load_kinds().get("generated_marker")
    leak_terms = contract.load_kinds().get("normative_leak_terms") or []
    leak_re = re.compile(r"\b(" + "|".join(re.escape(t) for t in leak_terms) + r")\b") \
        if leak_terms else None

    files = sorted(glob.glob(repo.path("docs/**/*.md"), recursive=True)
                   + glob.glob(repo.path("spec/**/*.md"), recursive=True))

    errors: list[str] = []
    warns: list[str] = []
    generated: list[str] = []
    slug_cache: dict[str, set[str]] = {}

    def slugs_of(relfile: str) -> set[str]:
        if relfile not in slug_cache:
            p = repo.path(relfile)
            slug_cache[relfile] = headings_slugs(open(p, encoding="utf-8").read()) \
                if os.path.isfile(p) else set()
        return slug_cache[relfile]

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
        if not rel.startswith(SRC_PATH_SKIP):
            for lineno, sp in stale_source_paths(text, lambda q: os.path.exists(repo.path(q))):
                warns.append(f"{rel}:{lineno}: source path '{sp}' does not exist")

        # normative leak (advisory)
        if leak_re and rel.startswith("docs/") and not rel.startswith(LEAK_SKIP):
            for lineno, line in _prose_lines(text):
                # ignore inline code spans
                stripped = re.sub(r"`[^`]*`", "", line)
                m = leak_re.search(stripped)
                if m:
                    warns.append(f"{rel}:{lineno}: normative '{m.group(1)}' in docs prose "
                                 f"(belongs in spec/ if it is a contract)")

    render.heading("docs-check")
    render.field("Files scanned", str(len(files)))
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
        render.fail(f"docs-check: FAIL ({len(errors)} broken anchors)")
        return 1
    render.ok(f"docs-check: PASS" + (f" ({len(warns)} warnings)" if warns else ""))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
