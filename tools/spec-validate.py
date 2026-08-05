#!/usr/bin/env python3
"""
spec-validate — reference implementation of the D-022 `spec validate` checker.

Runs from the repo root. Enforces the doc-system discipline (one fact one home relies on it):
  1. YAML parses; PRD/POLICY enums are within their closed sets; no duplicate F-/C-/ENG- IDs.
  2. PRD component `depends_on` resolve AND respect layer order
     (kernel < input < tui < workspace < plugin < remote — a component may not depend on a later layer).
  3. PRD feature `depends_on` resolve to a component or feature.
  4. POLICY `invariants:` resolve to the INV-* registry (docs/invariants/reference-invariants.md).
  5. POLICY `antipattern.refs` resolve to a real anti-pattern catalog ID (docs/anti-patterns).
  6. context-profiles `include` resolve to a known ID (F/C/ENG/ARCH/D/INV) or an existing file.
  7. Every Markdown relative link under spec/ and docs/ resolves.

Exit code 0 = PASS, 1 = FAIL. This is intentionally precise (no fuzzy free-text ref checking) so it has
zero false positives; parity-ID and DECISIONS free-text refs are left to review. See
docs/operations/spec-validate.md for the full spec.
"""
import sys, os, re, glob
try:
    import yaml
except ImportError:
    print("FAIL: PyYAML not installed (pip install pyyaml)"); sys.exit(1)

ROOT = os.getcwd()
errors, warnings = [], []
def err(m): errors.append(m)
def warn(m): warnings.append(m)

# ---------- load YAML ----------
def load(p):
    try:
        return yaml.safe_load(open(p))
    except Exception as e:
        err(f"YAML parse {p}: {e}"); return None

prd = load("spec/PRD.yaml") or {}
pol = load("spec/POLICY.yaml") or {}
prof = load("spec/context-profiles.yaml") or {}
gloss = load("spec/glossary.yaml") or {}
caps = load("spec/capabilities.yaml") or {}
deps = load("spec/dependencies.yaml") or {}
cfg  = load("spec/config-schema.yaml") or {}
CAP = set((caps.get("capabilities") or {}).keys())
DEP = set((deps.get("dependencies") or {}).keys())

# ---------- build ID registries ----------
def read(p): return open(p, encoding="utf-8").read()

# INV-* registry
INV = set(re.findall(r"\*\*(INV-[A-Z0-9-]+)\*\*", read("docs/invariants/reference-invariants.md")))
# ENG-* from POLICY
ENG = set((pol.get("principles") or {}).keys())
# D-* from DECISIONS
D = set(re.findall(r"^## (D-\d+)", read("spec/DECISIONS.md"), re.M))
# ARCH-* from ARCHITECTURE
ARCH = set(re.findall(r"\b(ARCH-[A-Z]+-\d+)\b", read("spec/ARCHITECTURE.md")))
# F-/C- from PRD
FEAT = set((prd.get("features") or {}).keys())
COMP = set((prd.get("components") or {}).keys())

# anti-pattern catalog: `## CODE — Name` (ALL-CAPS code) then a numbered list gives CODE-1..CODE-N
AP = {}  # category -> count
cur = None
for line in read("docs/anti-patterns/anti-patterns.md").splitlines():
    m = re.match(r"^## ([A-Z0-9][A-Z0-9-]+) — ", line)  # em-dash
    if m:
        cur = m.group(1); AP.setdefault(cur, 0); continue
    if cur and re.match(r"^\d+\.\s", line):
        AP[cur] += 1
def ap_ok(ref):
    m = re.match(r"^([A-Z0-9-]+)-(\d+)$", ref)
    if not m: return False
    cat, n = m.group(1), int(m.group(2))
    return cat in AP and 1 <= n <= AP[cat]

# ---------- 1. enums + duplicate IDs ----------
STAGE={"mvp","post-mvp","future"}; PRIO={"must","should","could"}
STATUS={"planned","active","blocked","done","dropped"}
STRENGTH={"required","recommended","situational"}; PSTATUS={"active","dropped"}
for fid,f in (prd.get("features") or {}).items():
    if f.get("stage") not in STAGE: err(f"PRD {fid}: bad stage {f.get('stage')}")
    if f.get("priority") not in PRIO: err(f"PRD {fid}: bad priority {f.get('priority')}")
    if f.get("status") not in STATUS: err(f"PRD {fid}: bad status {f.get('status')}")
for pid,pr in (pol.get("principles") or {}).items():
    if pr.get("strength","recommended") not in STRENGTH: err(f"POLICY {pid}: bad strength")
    if pr.get("status","active") not in PSTATUS: err(f"POLICY {pid}: bad status")
# duplicate IDs (yaml maps already unique; check raw text for accidental dup keys)
for path,pat in [("spec/PRD.yaml", r"^  (F-\d+):"), ("spec/PRD.yaml", r"^  (C-[A-Z]+): "),
                 ("spec/DECISIONS.md", r"^## (D-\d+)")]:
    ids = re.findall(pat, read(path), re.M)
    dup = {x for x in ids if ids.count(x) > 1}
    if dup: err(f"{path}: duplicate IDs {sorted(dup)}")

# ---------- 2/3. PRD dependency graph ----------
LAYER = ["kernel","input","tui","workspace","plugin","remote"]
LI = {l:i for i,l in enumerate(LAYER)}
for cid,c in (prd.get("components") or {}).items():
    if c.get("layer") not in LI: err(f"PRD {cid}: bad layer {c.get('layer')}")
    for d in c.get("depends_on",[]) or []:
        if d not in COMP: err(f"PRD {cid}: depends_on missing {d}")
        elif LI.get(c.get("layer"),99) < LI.get((prd['components'][d]).get("layer"),0):
            err(f"PRD {cid}({c.get('layer')}) depends on later-layer {d}({prd['components'][d].get('layer')})")
for fid,f in (prd.get("features") or {}).items():
    for d in f.get("depends_on",[]) or []:
        if d not in COMP and d not in FEAT: err(f"PRD {fid}: depends_on missing {d}")

# ---------- 4/5. POLICY refs ----------
for pid,pr in (pol.get("principles") or {}).items():
    for inv in pr.get("invariants",[]) or []:
        if inv not in INV: err(f"POLICY {pid}: invariants -> unknown {inv}")
    ap = (pr.get("antipattern") or {}).get("refs",[]) or []
    for r in ap:
        if not ap_ok(r): err(f"POLICY {pid}: antipattern ref -> unknown {r}")

# ---------- 5b. capabilities.yaml + dependencies.yaml ----------
CAP_ENUMS = {
    "product_layer": {"base","official-pack","third-party"},
    "architecture_layer": {"kernel","service","bundled-extension","external-plugin","external-tool"},
    "implementation": {"own","wrapped","direct","external-process"},
    "runtime": {"client","workspace","both"},
    "activation": {"startup","workspace","language","command","on-demand"},
    "trust": {"core","official","untrusted"},
}
for cid,c in (caps.get("capabilities") or {}).items():
    for k,vs in CAP_ENUMS.items():
        if k in c and c[k] not in vs: err(f"capabilities {cid}: bad {k}={c[k]}")
    for r in c.get("requires",[]) or []:
        if r not in CAP: err(f"capabilities {cid}: requires unknown {r}")
    for d in c.get("dep",[]) or []:
        if d not in DEP: err(f"capabilities {cid}: dep unknown {d}")
    for f in c.get("prd",[]) or []:
        if f not in FEAT and f not in COMP: err(f"capabilities {cid}: prd link unknown {f}")
DEP_ENUMS = {"usage":{"own","wrapped","direct","external-process","tooling"},
             "criticality":{"low","medium","high","critical"},
             "public_api_exposure":{"none","limited"}}
for did,d in (deps.get("dependencies") or {}).items():
    for k,vs in DEP_ENUMS.items():
        if k in d and d[k] not in vs: err(f"dependencies {did}: bad {k}={d[k]}")
    if "tier" in d and d["tier"] not in {0,1,2,3,4}: err(f"dependencies {did}: bad tier {d['tier']}")

# ---------- 5c. component `ref:` file paths resolve (relative to spec/) ----------
for cid,c in (prd.get("components") or {}).items():
    r = c.get("ref")
    if r:
        target = os.path.normpath(os.path.join("spec", r.split("#")[0]))
        if not os.path.exists(target): err(f"PRD {cid}: ref does not resolve -> {r}")

# ---------- 5d. feature `trace.design` file paths resolve (verify=aspirational, not checked) ----------
for fid,f in (prd.get("features") or {}).items():
    for d in ((f.get("trace") or {}).get("design") or []):
        target = os.path.normpath(os.path.join("spec", d.split("#")[0]))
        if not os.path.exists(target): err(f"PRD {fid}: trace.design does not resolve -> {d}")

# ---------- 5e. Definition-of-Done readiness gate (development-model.md) ----------
# Every mvp / must feature must be product-ready-traced: a resolving trace.design + non-empty acceptance.
for fid,f in (prd.get("features") or {}).items():
    if f.get("stage") == "mvp" or f.get("priority") == "must":
        design = ((f.get("trace") or {}).get("design") or [])
        if not design: err(f"DoD: {fid} (mvp/must) has no trace.design (methodology: a section ref is not a design)")
        if not (f.get("acceptance") or []): err(f"DoD: {fid} (mvp/must) has no acceptance criteria")

# ---------- 5f. config-schema.yaml enums ----------
CFG_TYPE={"bool","int","string","enum","list","map"}
CFG_SCOPE={"user","workspace","machine"}
CFG_MERGE={"replace","append","set-union","deep-merge"}
for sid,s in (cfg.get("settings") or {}).items():
    if s.get("type") not in CFG_TYPE: err(f"config-schema {sid}: bad type {s.get('type')}")
    if "scope" in s and s["scope"] not in CFG_SCOPE: err(f"config-schema {sid}: bad scope {s.get('scope')}")
    if "merge" in s and s["merge"] not in CFG_MERGE: err(f"config-schema {sid}: bad merge {s.get('merge')}")
    if s.get("type")=="enum" and not s.get("enum"): err(f"config-schema {sid}: enum type without enum values")

# ---------- 6. context-profiles include ----------
KNOWN = FEAT|COMP|ENG|ARCH|D|INV
for name,p in (prof.get("profiles") or {}).items():
    for inc in p.get("include",[]) or []:
        if inc in KNOWN: continue
        # allow doc paths / anchors
        cand = os.path.normpath(os.path.join("spec", inc.split("#")[0]))
        if os.path.exists(cand) or os.path.exists(inc.split("#")[0]): continue
        if ap_ok(inc): continue
        warn(f"context-profiles {name}: include '{inc}' does not resolve to a known ID or file")

# ---------- 7. Markdown relative links ----------
mdfiles = glob.glob("spec/**/*.md", recursive=True) + glob.glob("docs/**/*.md", recursive=True)
for f in mdfiles:
    base = os.path.dirname(f)
    for m in re.finditer(r"\]\(([^)]+)\)", read(f)):
        link = m.group(1).split("#")[0].strip()
        if not link or link.startswith(("http","mailto:","//","`")): continue
        if not os.path.exists(os.path.normpath(os.path.join(base, link))):
            err(f"{f}: broken link -> {link}")

# ---------- report ----------
print(f"registries: INV={len(INV)} ENG={len(ENG)} D={len(D)} ARCH={len(ARCH)} "
      f"F={len(FEAT)} C={len(COMP)} CAP={len(CAP)} DEP={len(DEP)} "
      f"anti-pattern-categories={len(AP)} glossary-terms={len(gloss.get('terms',{}))}")
print(f"md files checked: {len(mdfiles)}")
for w in warnings: print("WARN:", w)
if errors:
    print(f"\nFAIL ({len(errors)} errors):")
    for e in errors: print("  ", e)
    sys.exit(1)
print("\nspec validate: PASS" + (f" ({len(warnings)} warnings)" if warnings else ""))
