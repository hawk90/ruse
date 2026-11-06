"""Where the dev tooling keeps local state (D-062, #1137).

Per-change state goes to `$(git rev-parse --git-path ruse/work)` (inside `.git`, per worktree) and the
upstream cache to `${XDG_CACHE_HOME:-~/.cache}/ruse-dev/`; nothing writes under `.ruse/`, which belongs to
the product's committed project config. Each test builds a throwaway git repo holding a copy of `tools/`
and `spec/` and drives the real CLI in a subprocess, so the module-level repo root is the temp repo.
"""
import os
import shutil
import subprocess
import sys
import tempfile
import unittest

from . import _support  # noqa: F401

TOOLS = _support.TOOLS
SRC_ROOT = os.path.dirname(TOOLS)


def _env(cache: str) -> dict:
    # Hooks (lefthook pre-push) export GIT_DIR & co.; they would point the temp repo's git at ours.
    env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
    env["XDG_CACHE_HOME"] = cache
    return env


class TempRepo:
    def __init__(self, base: str):
        self.base = base
        self.root = os.path.join(base, "repo")
        self.cache = os.path.join(base, "xdg-cache")
        self.env = _env(self.cache)
        os.makedirs(self.root)
        ignore = shutil.ignore_patterns("__pycache__", "*.pyc")
        shutil.copytree(TOOLS, os.path.join(self.root, "tools"), ignore=ignore)
        shutil.copytree(os.path.join(SRC_ROOT, "spec"), os.path.join(self.root, "spec"), ignore=ignore)
        shutil.copy(os.path.join(SRC_ROOT, ".gitignore"), self.root)   # the real one: no .ruse/ entry
        self.git("init", "-q", "-b", "main")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "seed")

    def git(self, *args: str, cwd: str | None = None) -> str:
        p = subprocess.run(
            ["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid",
             "-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false", *args],
            cwd=cwd or self.root, env=self.env, capture_output=True, text=True, timeout=60)
        if p.returncode != 0:
            raise AssertionError(f"git {args} failed: {p.stderr}")
        return p.stdout.strip()

    def ruse(self, *args: str, cwd: str | None = None) -> subprocess.CompletedProcess:
        cwd = cwd or self.root
        return subprocess.run([sys.executable, os.path.join(cwd, "tools", "ruse.py"), *args],
                              cwd=cwd, env=self.env, capture_output=True, text=True, timeout=120)

    def py(self, code: str, cwd: str | None = None) -> str:
        cwd = cwd or self.root
        p = subprocess.run([sys.executable, "-c", "import sys; sys.path.insert(0, 'tools')\n" + code],
                           cwd=cwd, env=self.env, capture_output=True, text=True, timeout=60)
        if p.returncode != 0:
            raise AssertionError(p.stderr)
        return p.stdout.strip()

    def git_path(self, cwd: str) -> str:
        out = self.git("rev-parse", "--git-path", "ruse/work", cwd=cwd)
        return os.path.realpath(out if os.path.isabs(out) else os.path.join(cwd, out))


def _write(path: str, text: str) -> None:
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(text)


class TestStatePaths(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp(prefix="ruse-state-")
        self.addCleanup(shutil.rmtree, self._tmp, True)
        self.r = TempRepo(self._tmp)

    WORK = "from rusekit import repo; print(repo.work_root())"
    CACHE = "from rusekit import repo; print(repo.cache_root())"

    def test_work_root_in_a_normal_checkout(self):
        got = os.path.realpath(self.r.py(self.WORK))
        self.assertEqual(got, self.r.git_path(self.r.root))
        self.assertEqual(got, os.path.realpath(os.path.join(self.r.root, ".git", "ruse", "work")))

    def test_work_root_is_per_worktree(self):
        wt = os.path.join(self._tmp, "wt")
        self.r.git("worktree", "add", "-q", "-b", "side", wt)
        got = os.path.realpath(self.r.py(self.WORK, cwd=wt))
        self.assertEqual(got, self.r.git_path(wt))
        self.assertNotEqual(got, os.path.realpath(self.r.py(self.WORK)))
        self.assertTrue(got.startswith(os.path.realpath(os.path.join(self.r.root, ".git", "worktrees"))))

    def test_cache_root_follows_xdg_and_is_shared(self):
        wt = os.path.join(self._tmp, "wt")
        self.r.git("worktree", "add", "-q", "-b", "side", wt)
        want = os.path.join(self.r.cache, "ruse-dev")
        self.assertEqual(self.r.py(self.CACHE), want)
        self.assertEqual(self.r.py(self.CACHE, cwd=wt), want)

    def test_start_and_render_never_create_dot_ruse(self):
        p = self.r.ruse("change", "start", "--issue", "42", "--kind", "docs-editorial", "--goal", "g")
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        out = os.path.join(self._tmp, "body.md")
        p = self.r.ruse("pr", "render", "--issue", "42", "--out", out)
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        with open(out, encoding="utf-8") as fh:
            self.assertIn("ruse-gate:v1", fh.read())
        self.assertTrue(os.path.isfile(os.path.join(self.r.git_path(self.r.root), "42", "change.yaml")))
        self.assertFalse(os.path.exists(os.path.join(self.r.root, ".ruse")))
        # and the state never shows up as an untracked change
        self.assertEqual(self.r.git("status", "--porcelain"), "")

    def test_migration_moves_legacy_work_and_cache_once(self):
        old = os.path.join(self.r.root, ".ruse")
        _write(os.path.join(old, "work", "7", "change.yaml"), 'issue: "7"\nkind: "docs-editorial"\n')
        _write(os.path.join(old, "cache", "parity", "helix", "README"), "upstream\n")
        _write(os.path.join(old, "config.toml"), "# product config stays\n")
        p = self.r.ruse("status")
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertIn("moved .ruse/work/7", p.stderr)
        self.assertIn("moved .ruse/cache/parity/helix", p.stderr)
        self.assertTrue(os.path.isfile(os.path.join(self.r.git_path(self.r.root), "7", "change.yaml")))
        self.assertTrue(os.path.isfile(os.path.join(self.r.cache, "ruse-dev", "parity", "helix", "README")))
        self.assertFalse(os.path.exists(os.path.join(old, "work")))
        self.assertFalse(os.path.exists(os.path.join(old, "cache")))
        self.assertTrue(os.path.isfile(os.path.join(old, "config.toml")))   # not tooling state
        self.assertIn("Active issue", p.stdout)                              # found at the new path
        again = self.r.ruse("status")
        self.assertNotIn("moved", again.stderr)

    def test_migration_in_a_worktree_with_in_flight_contract(self):
        wt = os.path.join(self._tmp, "wt")
        self.r.git("worktree", "add", "-q", "-b", "side", wt)
        _write(os.path.join(wt, ".ruse", "work", "9", "change.yaml"), 'issue: "9"\n')
        p = self.r.ruse("status", cwd=wt)
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertTrue(os.path.isfile(os.path.join(self.r.git_path(wt), "9", "change.yaml")))
        self.assertFalse(os.path.exists(os.path.join(wt, ".ruse")))
        self.assertFalse(os.path.exists(os.path.join(self.r.git_path(self.r.root), "9")))

    def test_migration_drops_a_cache_already_shared(self):
        _write(os.path.join(self.r.cache, "ruse-dev", "parity", "vim", "KEEP"), "shared\n")
        _write(os.path.join(self.r.root, ".ruse", "cache", "parity", "vim", "OLD"), "dup\n")
        p = self.r.ruse("status")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn("dropped", p.stderr)
        self.assertTrue(os.path.isfile(os.path.join(self.r.cache, "ruse-dev", "parity", "vim", "KEEP")))
        self.assertFalse(os.path.exists(os.path.join(self.r.root, ".ruse")))

    def test_migration_refuses_on_conflict(self):
        _write(os.path.join(self.r.git_path(self.r.root), "5", "change.yaml"), "new\n")
        legacy = os.path.join(self.r.root, ".ruse", "work", "5", "change.yaml")
        _write(legacy, "old\n")
        p = self.r.ruse("status")
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("refusing to use the old .ruse/work/", p.stderr)
        self.assertTrue(os.path.isfile(legacy))          # nothing destroyed

    def _contract(self, issue: str) -> dict:
        import yaml
        with open(os.path.join(self.r.git_path(self.r.root), issue, "change.yaml"), encoding="utf-8") as fh:
            return yaml.safe_load(fh)

    def test_start_flags_fill_the_contract(self):
        p = self.r.ruse("change", "start", "--issue", "11", "--kind", "implementation", "--area", "core",
                        "--goal", "g: with colon", "--allow", "docs/x.md", "--allow", "tools/",
                        "--forbid", "spec/DECISIONS.md", "--spec-ref", "C-TRANSACTION",
                        "--spec-ref", "INV-TXN", "--spec-ref", "D-062", "--spec-ref", "CAP-EDIT-CORE",
                        "--non-goal", "no rust", "--rfc", "RFC-0021", "--decision", "D-062")
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        c = self._contract("11")
        # explicit --allow IS the blast radius: --area core must not widen it with crates/core/
        self.assertEqual(c["allow_paths"], ["docs/x.md", "tools/"])
        self.assertEqual(c["affected"]["crates"], ["core"])
        self.assertEqual(c["forbid_paths"], ["spec/DECISIONS.md"])
        self.assertEqual(c["affected"]["requirements"], ["C-TRANSACTION"])
        self.assertEqual(c["affected"]["invariants"], ["INV-TXN"])
        self.assertEqual(c["affected"]["decisions"], ["D-062"])
        self.assertEqual(c["affected"]["capabilities"], ["CAP-EDIT-CORE"])
        self.assertEqual(c["artifacts"], {"issue": "11", "rfc": "RFC-0021", "decision": "D-062",
                                          "migration": False, "compat_reviewed": None})
        self.assertEqual(c["non_goals"], ["no rust"])
        self.assertEqual(c["goal"].strip(), "g: with colon")
        with open(os.path.join(self.r.git_path(self.r.root), "11", "change.yaml"), encoding="utf-8") as fh:
            self.assertIn("# CAP-*", fh.read())          # template comments survive
        out = os.path.join(self._tmp, "body.md")
        p = self.r.ruse("pr", "render", "--issue", "11", "--out", out)
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        with open(out, encoding="utf-8") as fh:
            self.assertIn('"docs/x.md"', fh.read())

    def test_start_seeds_allow_from_area_only_without_explicit_allow(self):
        p = self.r.ruse("change", "start", "--issue", "14", "--kind", "implementation", "--area", "core")
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertEqual(self._contract("14")["allow_paths"], ["crates/core/"])

    def test_change_set_appends_and_replaces(self):
        self.assertEqual(self.r.ruse("change", "start", "--issue", "12", "--allow", "a/").returncode, 0)
        p = self.r.ruse("change", "set", "--issue", "12", "--allow", "b/", "--allow", "a/",
                        "--spec-ref", "F-001", "--kind", "spec", "--goal", "new goal")
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        c = self._contract("12")
        self.assertEqual(c["allow_paths"], ["a/", "b/"])          # appended, duplicate skipped
        self.assertEqual(c["affected"]["requirements"], ["F-001"])
        self.assertEqual((c["kind"], c["goal"].strip()), ("spec", "new goal"))
        p = self.r.ruse("change", "set", "--issue", "12", "--replace", "--allow", "c/", "--rfc", "RFC-1")
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        c = self._contract("12")
        self.assertEqual(c["allow_paths"], ["c/"])
        self.assertEqual(c["affected"]["requirements"], ["F-001"])  # untouched list kept
        self.assertEqual(c["artifacts"]["rfc"], "RFC-1")

    def test_change_set_rejects_unknown_spec_ref_and_missing_contract(self):
        self.assertEqual(self.r.ruse("change", "start", "--issue", "13").returncode, 0)
        p = self.r.ruse("change", "set", "--issue", "13", "--spec-ref", "RFC-0021")
        self.assertEqual(p.returncode, 2, p.stdout)
        self.assertNotEqual(self.r.ruse("change", "set", "--issue", "99", "--allow", "x").returncode, 0)

    def test_concurrent_fetches_share_one_cache(self):
        # A local "upstream" pinned at its tip; two worktrees fetch it at once into the shared cache.
        up = os.path.join(self._tmp, "upstream")
        os.makedirs(up)
        self.r.git("init", "-q", cwd=up)
        _write(os.path.join(up, "f"), "x\n")
        self.r.git("add", "f", cwd=up)
        self.r.git("commit", "-q", "-m", "u", cwd=up)
        sha = self.r.git("rev-parse", "HEAD", cwd=up)
        _write(os.path.join(self.r.root, "spec", "parity", "upstreams.yaml"),
               f"upstreams:\n  local:\n    repo: {up}\n    revision: {sha}\n    version_label: t\n")
        self.r.git("commit", "-q", "-am", "pin", cwd=self.r.root)
        wt = os.path.join(self._tmp, "wt")
        self.r.git("worktree", "add", "-q", "-b", "side", wt)
        procs = [subprocess.Popen([sys.executable, os.path.join(d, "tools", "parity", "fetch.py"), "local"],
                                  cwd=d, env=self.r.env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                  text=True) for d in (self.r.root, wt)]
        outs = [p.communicate(timeout=120)[0] for p in procs]
        for p, out in zip(procs, outs):
            self.assertEqual(p.returncode, 0, out)
        clone = os.path.join(self.r.cache, "ruse-dev", "parity", "local")
        self.assertEqual(self.r.git("rev-parse", "HEAD", cwd=clone), sha)
        self.assertEqual(sorted(os.listdir(os.path.dirname(clone))), [".local.lock", "local"])
        self.assertFalse(os.path.exists(os.path.join(self.r.root, ".ruse")))


if __name__ == "__main__":
    unittest.main()
