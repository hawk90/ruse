"""PRD trace paths of finished work must exist (#886).

`trace.verify` (and `trace.contract`) list the files that prove a feature. For a `planned` feature they are
where that proof WILL live, so a missing path is expected. For a `status: done` feature they are the proof
chain of finished work: a path that does not resolve means the chain is fiction. spec-validate runs this.
Paths are relative to spec/ (like every PRD path); a `#fragment` or `::item` suffix is ignored.
"""
from __future__ import annotations

import os

CHECKED = ("verify", "contract")


def _target(spec_dir: str, ref: str) -> str:
    return os.path.normpath(os.path.join(spec_dir, ref.split("#")[0].split("::")[0]))


def done_trace_errors(features: dict, spec_dir: str) -> list[str]:
    errors: list[str] = []
    for fid, f in (features or {}).items():
        if not isinstance(f, dict) or f.get("status") != "done":
            continue
        trace = f.get("trace") or {}
        if not (trace.get("verify") or []):
            errors.append(f"PRD {fid}: status done but trace.verify is empty — name the tests that prove it")
        for key in CHECKED:
            for ref in trace.get(key) or []:
                if not isinstance(ref, str) or not os.path.exists(_target(spec_dir, ref)):
                    errors.append(f"PRD {fid}: status done but trace.{key} does not resolve -> {ref}")
    return errors
