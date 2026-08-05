"""rusekit — shared library for the ruse change-workflow toolchain (tools/ruse.py).

One home for the things every subcommand needs: repo paths and the changed-file set
(repo.py), consistent terminal output (render.py), the spec-registry + impact graph
(model.py), and the Change Contract schema (contract.py).

Design rules (mirror the repo's own ENG-DOC-001 "one fact, one home"):
  * Policy lives in spec/ YAML (spec/change-kinds.yaml), never hard-coded here.
  * These modules read the real spec registries (PRD/POLICY/capabilities/dependencies/
    DECISIONS/ARCHITECTURE/invariants); they never invent IDs.
  * Everything degrades gracefully when git or cargo is absent — a missing tool is a
    reported skip, never a crash.
"""

__all__ = ["repo", "render", "model", "contract"]
