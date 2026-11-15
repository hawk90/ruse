"""CI and `ruse verify` run the same checks (#1310, RA-RUSE-006).

CI does not call `ruse verify`: it splits the steps across jobs and caches them. So the match between
spec/verification.yaml and the PR workflows is checked here, both ways:

- every registry step has a CI counterpart (a `run:` with the same command, or the action its `ci:` names);
- every CI step that runs `cargo` or `python3` is a registry step or is listed under `ci_only` with a reason.
"""
import os
import shlex
import unittest

import yaml

from . import _support  # noqa: F401

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
REGISTRY = os.path.join(ROOT, "spec", "verification.yaml")
# The workflows that run on every PR. Scheduled and release workflows are not CI-equals-local surfaces.
WORKFLOWS = ("ci.yml", "spec-check.yml")


def _norm(argv: list[str]) -> tuple[str, ...]:
    """A command for comparison: `-q` only changes how much is printed."""
    return tuple(a for a in argv if a != "-q")


def _steps(workflow: str) -> list[dict]:
    with open(os.path.join(ROOT, ".github", "workflows", workflow), encoding="utf-8") as fh:
        wf = yaml.safe_load(fh)
    return [st for job in wf["jobs"].values() for st in job.get("steps", [])]


def _key(st: dict) -> str:
    return st.get("name") or st.get("id") or st.get("run", "").strip()


def _command(st: dict) -> tuple[str, ...] | None:
    """The argv of a one-line `run:` that calls cargo or python3, else None."""
    run = st.get("run")
    if not isinstance(run, str) or "\n" in run.strip():
        return None
    argv = shlex.split(run)
    return _norm(argv) if argv and argv[0] in ("cargo", "python3") else None


class TestCiMatchesRegistry(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        with open(REGISTRY, encoding="utf-8") as fh:
            reg = yaml.safe_load(fh)
        cls.steps = reg["steps"]
        cls.ci_only = reg.get("ci_only") or {}
        cls.ci = {wf: _steps(wf) for wf in WORKFLOWS}

    def test_every_registry_step_runs_in_ci(self):
        commands = {_command(st) for steps in self.ci.values() for st in steps}
        for step in self.steps:
            with self.subTest(step=step["name"]):
                if "ci" in step:
                    wf, _, name = step["ci"].partition("#")
                    self.assertIn(wf, self.ci, f"{step['name']}: ci names an unknown workflow")
                    self.assertIn(name, {st.get("name") for st in self.ci[wf]},
                                  f"{step['name']}: no step named {name!r} in {wf}")
                else:
                    self.assertIn(_norm(step["cmd"]), commands,
                                  f"{step['name']}: `{' '.join(step['cmd'])}` is not a CI `run:` line")

    def test_every_ci_command_is_a_registry_step_or_ci_only(self):
        registry = {_norm(s["cmd"]) for s in self.steps}
        for wf, steps in self.ci.items():
            for st in steps:
                run = st.get("run")
                if not isinstance(run, str):
                    continue
                cmd = _command(st)
                with self.subTest(workflow=wf, step=_key(st)):
                    if cmd is None and "\n" not in run.strip():
                        continue  # a one-line echo or similar: not a check
                    if cmd in registry:
                        continue
                    self.assertIn(_key(st), self.ci_only,
                                  f"{wf}: `{_key(st)}` runs a check that is not in spec/verification.yaml "
                                  "steps; add it there, or to ci_only with a reason")

    def test_ci_only_entries_exist_and_give_a_reason(self):
        keys = {_key(st) for steps in self.ci.values() for st in steps}
        for key, reason in self.ci_only.items():
            with self.subTest(key=key):
                self.assertIn(key, keys, "ci_only names a CI step that no longer exists")
                self.assertTrue(isinstance(reason, str) and reason.strip(), "ci_only entry needs a reason")


if __name__ == "__main__":
    unittest.main()
