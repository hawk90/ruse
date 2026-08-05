"""The spec registry as a graph.

Loads the real ruse registries and links them the way the specs already reference each
other, so `impact`, `context`, and `pr` share one source of truth:

  PRD.yaml         F-* / C-*     (features, components; depends_on, trace.design, ref)
  POLICY.yaml      ENG-*         (principles; invariants)
  capabilities.yaml CAP-*        (requires, dep, prd)
  dependencies.yaml DEP-*        (allowed_layers -> crates)
  DECISIONS.md     D-*           (Refs + in-body ID mentions)
  ARCHITECTURE.md  ARCH-*
  invariants/...   INV-*
  docs/rfc/*       RFC-*         (status by directory + frontmatter; in-body ID mentions)
  crates/*                       CRATE-* pseudo-nodes for the source tree

Nothing here hard-codes an ID list — it reads what the repo declares. Free-text refs are
matched by a strict ID regex, so a typo simply fails to link (no phantom edges).
"""
from __future__ import annotations

import os
import re
from dataclasses import dataclass, field

try:
    import yaml
except ImportError:  # pragma: no cover
    yaml = None

from . import repo

ID_RE = re.compile(
    r"\b(F-\d+|C-[A-Z]+|CAP-[A-Z0-9-]+|DEP-[A-Z0-9-]+|INV-[A-Z0-9-]+"
    r"|ARCH-[A-Z]+-\d+|ENG-[A-Z0-9-]+|D-\d+|RFC-\d{1,4})\b"
)

BUILD_STAGES = ["kernel", "input", "tui", "workspace", "plugin", "remote"]  # PRD build_stage axis (D-036)


@dataclass
class Node:
    id: str
    kind: str                       # F C ENG CAP DEP D ARCH INV RFC CRATE DOC
    title: str = ""
    file: str | None = None         # repo-relative "home" file, if any
    meta: dict = field(default_factory=dict)


@dataclass
class Edge:
    src: str
    rel: str                        # depends_on requires dep prd invariant layer ref design
    dst: str


class Model:
    def __init__(self) -> None:
        self.nodes: dict[str, Node] = {}
        self.edges: list[Edge] = []
        self._out: dict[str, list[Edge]] = {}
        self._in: dict[str, list[Edge]] = {}
        self.file_index: dict[str, set[str]] = {}   # repo-rel file -> ids that live/appear there
        self.errors: list[str] = []

    # ---- construction ----------------------------------------------------------
    def add_node(self, node: Node) -> None:
        if node.id not in self.nodes:
            self.nodes[node.id] = node
        else:
            # keep first title/file but let later passes fill blanks
            cur = self.nodes[node.id]
            if not cur.title and node.title:
                cur.title = node.title
            if not cur.file and node.file:
                cur.file = node.file
        if node.file:
            self.file_index.setdefault(node.file, set()).add(node.id)

    def add_edge(self, src: str, rel: str, dst: str) -> None:
        if src == dst:
            return
        e = Edge(src, rel, dst)
        self.edges.append(e)
        self._out.setdefault(src, []).append(e)
        self._in.setdefault(dst, []).append(e)

    def link_file(self, relfile: str, node_id: str) -> None:
        self.file_index.setdefault(relfile, set()).add(node_id)

    # ---- queries ---------------------------------------------------------------
    def has(self, node_id: str) -> bool:
        return node_id in self.nodes

    def title(self, node_id: str) -> str:
        n = self.nodes.get(node_id)
        return n.title if n else ""

    def neighbors(self, node_id: str, direction: str = "both",
                  rels: set[str] | None = None) -> list[Edge]:
        out: list[Edge] = []
        if direction in ("out", "both"):
            out += self._out.get(node_id, [])
        if direction in ("in", "both"):
            out += self._in.get(node_id, [])
        if rels is not None:
            out = [e for e in out if e.rel in rels]
        return out

    def bfs(self, roots: list[str], direction: str = "both",
            rels: set[str] | None = None, depth: int = 3) -> dict[str, int]:
        """Return {id: distance} reachable from roots within `depth` hops."""
        seen: dict[str, int] = {r: 0 for r in roots if r in self.nodes}
        frontier = list(seen.keys())
        d = 0
        while frontier and d < depth:
            d += 1
            nxt: list[str] = []
            for nid in frontier:
                for e in self.neighbors(nid, direction, rels):
                    other = e.dst if e.src == nid else e.src
                    if other in self.nodes and other not in seen:
                        seen[other] = d
                        nxt.append(other)
            frontier = nxt
        return seen

    def ids_for_file(self, relfile: str) -> set[str]:
        """IDs whose home is this file, plus (for crates) the crate pseudo-node."""
        ids = set(self.file_index.get(relfile, set()))
        crate = repo.crate_of(relfile)
        if crate:
            cid = f"CRATE-{crate}"
            if cid in self.nodes:
                ids.add(cid)
        return ids

    def files_for_id(self, node_id: str) -> list[str]:
        return [f for f, ids in self.file_index.items() if node_id in ids]


# ---- loading --------------------------------------------------------------------

def _load_yaml(relpath: str, m: Model):
    p = repo.path(relpath)
    if not os.path.isfile(p):
        return {}
    try:
        return yaml.safe_load(open(p, encoding="utf-8")) or {}
    except Exception as e:  # pragma: no cover
        m.errors.append(f"YAML parse {relpath}: {e}")
        return {}


def _read(relpath: str) -> str:
    p = repo.path(relpath)
    return open(p, encoding="utf-8").read() if os.path.isfile(p) else ""


def _spec_rel(ref: str) -> str:
    """Resolve a spec/-relative doc ref (as used in PRD/context-profiles) to repo-rel."""
    target = os.path.normpath(os.path.join("spec", ref.split("#")[0]))
    return target.replace(os.sep, "/")


def load() -> Model:
    m = Model()
    if yaml is None:
        m.errors.append("PyYAML not installed (pip install pyyaml)")
        return m

    # crates as pseudo-nodes
    for crate in repo.CRATES:
        if os.path.isdir(repo.path("crates", crate)):
            m.add_node(Node(f"CRATE-{crate}", "CRATE", title=f"crate {crate}",
                            file=f"crates/{crate}"))

    # PRD.yaml — components + features
    prd = _load_yaml("spec/PRD.yaml", m)
    for cid, c in (prd.get("components") or {}).items():
        m.add_node(Node(cid, "C", title=c.get("title", ""), file="spec/PRD.yaml",
                        meta={"build_stage": c.get("build_stage"), "status": c.get("status")}))
    for fid, f in (prd.get("features") or {}).items():
        m.add_node(Node(fid, "F", title=f.get("title", ""), file="spec/PRD.yaml",
                        meta={"stage": f.get("stage"), "status": f.get("status")}))
    for cid, c in (prd.get("components") or {}).items():
        for dep in c.get("depends_on") or []:
            m.add_edge(cid, "depends_on", dep)
        if c.get("ref"):
            doc = _spec_rel(c["ref"])
            m.add_node(Node(doc, "DOC", title=os.path.basename(doc), file=doc))
            m.add_edge(cid, "ref", doc)
            m.link_file(doc, cid)
    for fid, f in (prd.get("features") or {}).items():
        for dep in f.get("depends_on") or []:
            m.add_edge(fid, "depends_on", dep)
        for doc in ((f.get("trace") or {}).get("design") or []):
            d = _spec_rel(doc)
            m.add_node(Node(d, "DOC", title=os.path.basename(d), file=d))
            m.add_edge(fid, "design", d)
            m.link_file(d, fid)

    # POLICY.yaml — principles
    pol = _load_yaml("spec/POLICY.yaml", m)
    for pid, pr in (pol.get("principles") or {}).items():
        m.add_node(Node(pid, "ENG", title=pr.get("title", ""), file="spec/POLICY.yaml"))
        for inv in pr.get("invariants") or []:
            m.add_edge(pid, "invariant", inv)

    # invariants registry (targets of ENG.invariant edges)
    for inv in re.findall(r"\*\*(INV-[A-Z0-9-]+)\*\*",
                          _read("docs/invariants/reference-invariants.md")):
        m.add_node(Node(inv, "INV", file="docs/invariants/reference-invariants.md"))

    # capabilities.yaml
    caps = _load_yaml("spec/capabilities.yaml", m)
    for cid, c in (caps.get("capabilities") or {}).items():
        m.add_node(Node(cid, "CAP", title=c.get("title", ""),
                        file="spec/capabilities.yaml"))
    for cid, c in (caps.get("capabilities") or {}).items():
        for r in c.get("requires") or []:
            m.add_edge(cid, "requires", r)
        for d in c.get("dep") or []:
            m.add_edge(cid, "dep", d)
        for p in c.get("prd") or []:
            m.add_edge(cid, "prd", p)

    # dependencies.yaml — DEP allowed in crates
    deps = _load_yaml("spec/dependencies.yaml", m)
    for did, d in (deps.get("dependencies") or {}).items():
        m.add_node(Node(did, "DEP", title=str(d.get("purpose", "")),
                        file="spec/dependencies.yaml"))
        for layer in d.get("allowed_layers") or []:
            cid = f"CRATE-{layer}"
            if cid in m.nodes:
                m.add_edge(did, "layer", cid)

    # ARCHITECTURE.md — ARCH-* ids
    arch_txt = _read("spec/ARCHITECTURE.md")
    for aid in set(re.findall(r"\b(ARCH-[A-Z]+-\d+)\b", arch_txt)):
        m.add_node(Node(aid, "ARCH", file="spec/ARCHITECTURE.md"))

    # DECISIONS.md — D-* + referenced ids
    dec_txt = _read("spec/DECISIONS.md")
    blocks = re.split(r"(?m)^## (D-\d+)", dec_txt)
    # blocks = [pre, id1, body1, id2, body2, ...]
    for i in range(1, len(blocks), 2):
        did = blocks[i]
        title_line = blocks[i + 1].splitlines()[0] if i + 1 < len(blocks) else ""
        title = re.sub(r"^\s*—\s*", "", title_line).split(" · ")[0].strip()
        m.add_node(Node(did, "D", title=title, file="spec/DECISIONS.md"))
        body = blocks[i + 1] if i + 1 < len(blocks) else ""
        for ref in set(ID_RE.findall(body)):
            if ref != did and ref in m.nodes:
                m.add_edge(did, "ref", ref)

    # RFCs — id + status(dir/frontmatter) + in-body ID mentions
    import glob
    for p in glob.glob(repo.path("docs/rfc/**/*.md"), recursive=True):
        base = os.path.basename(p)
        mo = re.match(r"(RFC-\d{1,4})", base)
        if not mo:
            continue
        rid = mo.group(1)
        relf = repo.rel(p)
        lifecycle = relf.split("/")[2] if len(relf.split("/")) > 2 else "proposed"
        txt = _read(relf)
        fmst = re.search(r"(?m)^status:\s*(\S+)", txt)
        title = re.search(r"(?m)^title:\s*\"?([^\"]+)", txt)
        m.add_node(Node(rid, "RFC", title=(title.group(1).strip() if title else ""),
                        file=relf,
                        meta={"lifecycle": lifecycle,
                              "status": fmst.group(1) if fmst else lifecycle}))
        for ref in set(ID_RE.findall(txt)):
            if ref != rid and ref in m.nodes:
                m.add_edge(rid, "ref", ref)

    return m
