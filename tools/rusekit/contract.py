"""The Change Contract (`.ruse/work/<id>/change.yaml`) and the change-kinds policy.

The contract is the small, local, per-task agreement: what kind of change this is, what
it may touch, and what evidence closes it. It is NOT permanent state — only the final
RFC/Decision/PRD/PR is. The authoritative taxonomy it references lives in
spec/change-kinds.yaml (one fact, one home).
"""
from __future__ import annotations

import os

try:
    import yaml
except ImportError:  # pragma: no cover
    yaml = None

from . import repo

KIND_POLICY = "spec/change-kinds.yaml"

# Fields the contract may carry. affected.* hold spec IDs; contracts.* are booleans.
CONTRACT_KEYS = {"issue", "kind", "goal", "non_goals", "affected", "contracts",
                 "artifacts", "allow_paths", "forbid_paths", "evidence", "branch"}


def load_kinds() -> dict:
    p = repo.path(KIND_POLICY)
    if yaml is None or not os.path.isfile(p):
        return {}
    return yaml.safe_load(open(p, encoding="utf-8")) or {}


def kind_names() -> list[str]:
    return list((load_kinds().get("kinds") or {}).keys())


def kind_risk(kind: str) -> int | None:
    k = (load_kinds().get("kinds") or {}).get(kind)
    return None if k is None else k.get("risk")


# ---- change.yaml ----------------------------------------------------------------

def contract_path(issue: str) -> str:
    return os.path.join(repo.work_dir(issue), "change.yaml")


def load(issue: str) -> dict | None:
    p = contract_path(issue)
    if yaml is None or not os.path.isfile(p):
        return None
    return yaml.safe_load(open(p, encoding="utf-8")) or {}


def affected_ids(contract: dict) -> list[str]:
    aff = contract.get("affected") or {}
    ids: list[str] = []
    for key in ("capabilities", "requirements", "invariants", "decisions"):
        ids += aff.get(key) or []
    return ids


def declared_paths(contract: dict) -> list[str]:
    """Paths the contract itself names (allow_paths + affected.crates as crates/<c>/)."""
    out = list(contract.get("allow_paths") or [])
    for crate in (contract.get("affected") or {}).get("crates") or []:
        out.append(f"crates/{crate}/")
    return out


def validate(contract: dict, model) -> tuple[list[str], list[str]]:
    """Structural + referential validation. Returns (errors, warnings)."""
    errors: list[str] = []
    warnings: list[str] = []

    unknown = set(contract) - CONTRACT_KEYS
    if unknown:
        warnings.append(f"unknown change.yaml keys: {sorted(unknown)}")

    kind = contract.get("kind")
    kinds = kind_names()
    if not kind:
        errors.append("change.yaml: missing 'kind'")
    elif kinds and kind not in kinds:
        errors.append(f"change.yaml: kind '{kind}' not in {kinds}")

    if not (contract.get("goal") or "").strip():
        warnings.append("change.yaml: empty 'goal'")

    # affected IDs must resolve in the spec model
    for nid in affected_ids(contract):
        if model.has(nid):
            continue
        errors.append(f"change.yaml: affected id '{nid}' is not a known spec ID")

    # crates must exist
    for crate in (contract.get("affected") or {}).get("crates") or []:
        if crate not in repo.CRATES:
            warnings.append(f"change.yaml: affected crate '{crate}' is not a known crate")

    # artifact refs, if given, must resolve
    art = contract.get("artifacts") or {}
    for key in ("rfc", "decision"):
        ref = art.get(key)
        if ref and not model.has(ref):
            warnings.append(f"change.yaml: artifacts.{key} '{ref}' does not resolve")

    return errors, warnings
