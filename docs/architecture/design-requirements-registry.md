# Design-requirement registry (DR-*)

[`spec/design-requirements.yaml`](../../spec/design-requirements.yaml) is the **source of truth for the status of the long-horizon design requirements (DR-\*)**
(상태·목록의 정본). [`docs/architecture/design-requirements.md`](design-requirements.md) keeps
only the narrative: why each of the 20 domains matters. Don't put requirement lists or ✅/❌ status there.

**Scope.** This registry tracks DR-\* design requirements only. Product features (F-\*), components (C-\*),
capabilities, config keys and decisions keep their single home in `spec/` (`PRD.yaml`, `capabilities.yaml`,
`config-schema.yaml`, `DECISIONS.md`). Don't copy F-\* state here. Link to it from `evidence`/`notes`.

## Schema (`schema: 1`)

Each entry in `items:` has these fields, in this order. The file was written with PyYAML (`sort_keys=False`,
`allow_unicode`, indented lists, `width=120`). Keep that order when editing by hand.

| Field | Required | Meaning |
|---|---|---|
| `id` | yes | `<AREA>-<NNN>`, e.g. `PERSIST-003`. **Stable**: never renumber or reuse an id. A new requirement takes the next free number in its area. |
| `title` | yes | Short English name. |
| `area` | yes | Lowercase domain code: `spec` `par` `persist` `det` `sched` `cache` `id` `multi` `gov` `cfg` `stab` `trust` `xplat` `tux` `rir` `apix` `perfs` `ops` `contrib` `scope` (§1–§20 of design-requirements.md; same codes as the anti-pattern catalog). |
| `priority` | yes | `P0`..`P3` from the source doc (P0 foundational/data-integrity/core-path · P1 major-subsystem correctness · P2 quality · P3 long-horizon/polish). |
| `priority_inferred` | no | `true` when no source gave a priority (omit otherwise; no current item uses it). |
| `status` | yes | `done` · `partial` · `todo` · `dropped` (see below). |
| `phase` | no | A `spec/phases.yaml` id (`ecosystem`, `breadth`, …) when the requirement only becomes actionable with a later-phase feature. |
| `acceptance` | yes | List of concrete, checkable criteria. |
| `evidence` | yes | Repo paths proving the status: `path`, `path:line` or `path::symbol` (the symbol must appear in that file). Prefer the implementing function, a test, a tool/CI gate; a design doc is acceptable evidence only for `partial`. Must not be empty for `done`/`partial`; `[]` for `todo`. Every path must exist. |
| `legacy_ids` | yes | The original `DR-<CODE>-<n>` id. Other docs, RFCs and commits use it, so grep for it. |
| `source` | yes | List of `"design-requirements.md §<n> <domain>"`. |
| `notes` | no | Gaps, caveats and resolved conflicts ("doc said X; code: Y"). Required for `dropped`. |

## Status meanings

- **done**: the requirement holds in the repo today.
  - Process/spec/governance requirement: the governing artifact exists **and** is enforced or in use (a
    validator, CI job, `ruse` gate, template, or recorded non-goal the code respects).
  - Runtime requirement: implemented in `crates/core` or `apps/tui` and covered by a test.
- **partial**: specified or designed (docs/design, spec/, RFC, D-record) but not implemented or enforced;
  or implemented for only part of the acceptance.
- **todo**: not addressed anywhere beyond the requirement text.
- **dropped**: intentionally out of scope or obsolete; the reason goes in `notes`.

Code wins over prose. If a design doc claims something the code doesn't do, the status follows the code and
the difference goes in `notes`.

## How to update (agents and humans)

1. Find the item: `grep -n "DR-PERSIST-3\|<keyword>" spec/design-requirements.yaml`. Edit it in place and keep the
   `id`.
2. When a status changes, update `evidence` with real paths (a test name beats a file) and give the reason
   briefly in `notes`. Verify against the code, not against another doc.
3. For a new requirement, add it to its area block with the next free number, and fill in `legacy_ids` (the
   new `DR-` id) and `source`. Add the matching narrative to design-requirements.md only if it changes the
   domain's rationale.
4. Refresh the status-summary table in design-requirements.md.
5. Validate from the repo root:
   ```bash
   python3 - <<'EOF'
   import yaml, os, re, collections
   d = yaml.safe_load(open('spec/design-requirements.yaml')); items = d['items']; ids = [i['id'] for i in items]
   assert len(ids) == len(set(ids)), [k for k, v in collections.Counter(ids).items() if v > 1]
   bad = []
   for i in items:
       for e in i.get('evidence') or []:
           p, _, sym = str(e).partition('::'); p = re.sub(r':\d+(-\d+)?$', '', p)
           if not os.path.exists(p) or (sym and sym not in open(p).read()): bad.append((i['id'], e))
       if i['status'] in ('done', 'partial') and not i.get('evidence'): bad.append((i['id'], 'no evidence'))
   print(len(ids), 'items; bad evidence:', bad)
   print(collections.Counter(i['status'] for i in items))
   EOF
   ruby -ryaml -rdate -e 'YAML.load_file(ARGV[0], permitted_classes: [Date])' spec/design-requirements.yaml
   python3 tools/spec-validate.py
   ```
6. Bump `updated:` at the top of the file.

Snapshot on 2026-10-05 (after deep verification): 111 items, with 21 done, 77 partial and 13 todo.
