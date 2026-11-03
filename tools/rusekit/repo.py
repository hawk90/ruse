"""Repo location, the changed-file set, and where the dev tooling keeps local state.

Per-change state lives in `$(git rev-parse --git-path ruse/work)/<id>/` (inside `.git`, per worktree,
never committed) and the upstream-source cache in `${XDG_CACHE_HOME:-~/.cache}/ruse-dev/` (shared across
worktrees). `.ruse/` at the root belongs to the product's committed project config (D-062), so nothing
here writes there; a legacy `.ruse/work` or `.ruse/cache` is moved out on first use (#1137).

The change tooling is git-aware but not git-required. `changed_files()` returns None
when this is not a git checkout (callers then fall back to an explicit --files list or
change.yaml's declared paths), so no command hard-crashes outside git.
"""
from __future__ import annotations

import errno
import hashlib
import os
import shutil
import subprocess
import sys
from dataclasses import dataclass, field


# ---- repo root ------------------------------------------------------------------

def find_root(start: str | None = None) -> str:
    """Walk up from `start` (or cwd) to the dir that has both spec/ and tools/."""
    cur = os.path.abspath(start or os.getcwd())
    while True:
        if os.path.isdir(os.path.join(cur, "spec")) and os.path.isdir(os.path.join(cur, "tools")):
            return cur
        parent = os.path.dirname(cur)
        if parent == cur:
            # Fall back to cwd; the caller's relative paths still resolve there.
            return os.path.abspath(start or os.getcwd())
        cur = parent


ROOT = find_root()


def path(*parts: str) -> str:
    return os.path.join(ROOT, *parts)


def rel(p: str) -> str:
    """Repo-relative, forward-slash path (stable across OSes for policy matching)."""
    return os.path.relpath(os.path.abspath(p), ROOT).replace(os.sep, "/")


# ---- git ------------------------------------------------------------------------

def _git(*args: str) -> tuple[int, str]:
    try:
        p = subprocess.run(
            ["git", *args], cwd=ROOT, capture_output=True, text=True, timeout=30
        )
        return p.returncode, (p.stdout or "")
    except (FileNotFoundError, subprocess.SubprocessError):
        return 127, ""


def is_git_repo() -> bool:
    code, out = _git("rev-parse", "--is-inside-work-tree")
    return code == 0 and out.strip() == "true"


def current_branch() -> str | None:
    if not is_git_repo():
        return None
    code, out = _git("rev-parse", "--abbrev-ref", "HEAD")
    return out.strip() if code == 0 else None


def changed_files(base: str | None = None) -> list[str] | None:
    """Repo-relative paths changed in the working tree.

    Returns None when this is not a git repo. With `base` (e.g. origin/main) the set
    is (base..working-tree): committed diff + staged + unstaged + untracked.
    Without `base` it is everything not committed at HEAD + untracked.
    """
    if not is_git_repo():
        return None
    files: set[str] = set()

    def add(code_out: tuple[int, str]):
        code, out = code_out
        if code == 0:
            for line in out.splitlines():
                line = line.strip()
                if line:
                    files.add(line.replace(os.sep, "/"))

    if base:
        add(_git("diff", "--name-only", f"{base}...HEAD"))
    add(_git("diff", "--name-only", "HEAD"))            # staged + unstaged vs HEAD
    add(_git("ls-files", "--others", "--exclude-standard"))  # untracked
    return sorted(files)


# ---- local state layout (D-062) -------------------------------------------------

LEGACY_DIR = ".ruse"   # the product's project-config dir; the tooling no longer writes here
_work_root: str | None = None
_migrated = False


def _xdg(var: str, fallback: str) -> str:
    v = os.environ.get(var)
    return v if v and os.path.isabs(v) else os.path.join(os.path.expanduser("~"), fallback)


def _cache_root() -> str:
    return os.path.join(_xdg("XDG_CACHE_HOME", ".cache"), "ruse-dev")


def cache_root() -> str:
    """`${XDG_CACHE_HOME:-~/.cache}/ruse-dev` — the upstream-source cache, shared across worktrees."""
    migrate_legacy()
    return _cache_root()


def _resolve_work_root() -> str:
    code, out = _git("rev-parse", "--git-path", "ruse/work")
    if code == 0 and out.strip():
        p = out.strip()
        return os.path.normpath(p if os.path.isabs(p) else os.path.join(ROOT, p))
    # Not a git checkout: the per-user state dir, keyed by the root so two checkouts never collide.
    key = hashlib.sha256(ROOT.encode("utf-8")).hexdigest()[:16]
    return os.path.join(_xdg("XDG_STATE_HOME", os.path.join(".local", "state")), "ruse-dev", key, "work")


def work_root() -> str:
    """`$(git rev-parse --git-path ruse/work)` — per worktree, inside `.git`, never committed."""
    global _work_root
    if _work_root is None:
        _work_root = _resolve_work_root()
    migrate_legacy()
    return _work_root


def work_dir(issue: str) -> str:
    return os.path.join(work_root(), str(issue))


def show(p: str) -> str:
    """A path for humans: repo-relative when inside the checkout, absolute otherwise."""
    r = rel(p)
    return os.path.abspath(p) if r == ".." or r.startswith("../") else r


class LegacyStateError(SystemExit):
    """The old `.ruse/` state could not be moved; the tooling refuses to run on the old path."""


def _remove(p: str) -> None:
    (shutil.rmtree if os.path.isdir(p) and not os.path.islink(p) else os.remove)(p)


def _move(src: str, dst: str) -> None:
    """Move a file or tree to `dst` (which must not exist). Atomic within a filesystem; across
    filesystems it copies to a sibling temp name and renames, so `dst` never appears half-written."""
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    try:
        os.rename(src, dst)
        return
    except OSError as e:
        if e.errno != errno.EXDEV:
            raise
    tmp = f"{dst}.tmp-{os.getpid()}"
    if os.path.isdir(src) and not os.path.islink(src):
        shutil.copytree(src, tmp, symlinks=True)
    else:
        shutil.copy2(src, tmp, follow_symlinks=False)
    os.rename(tmp, dst)
    _remove(src)


def _say(msg: str) -> None:
    print(f"ruse: {msg}", file=sys.stderr)


def migrate_legacy() -> list[tuple[str, str]]:
    """Move pre-D-062 tooling state out of `.ruse/` once; returns the (old, new) pairs moved.

    `.ruse/work/<id>/` → work_root()/<id>/ (refuses when an id exists in both: resolve by hand);
    `.ruse/context.md` + `context-lock.json` (ad-hoc packs) → next to work_root();
    `.ruse/cache/<sub>/<name>` → cache_root()/<sub>/<name> (the shared cache may already hold that
    upstream from another worktree: the local duplicate is then dropped). Anything else under `.ruse/`
    (product config, D-062) is left alone. Runs at most once per process."""
    global _migrated
    if _migrated:
        return []
    _migrated = True
    legacy = path(LEGACY_DIR)
    if not os.path.isdir(legacy):
        return []
    moved: list[tuple[str, str]] = []
    wroot = work_root()

    old_work = os.path.join(legacy, "work")
    if os.path.isdir(old_work):
        names = sorted(os.listdir(old_work))
        clash = [n for n in names if os.path.lexists(os.path.join(wroot, n))]
        if clash:
            raise LegacyStateError(
                f"ruse: refusing to use the old {LEGACY_DIR}/work/: {', '.join(clash)} exist(s) in both "
                f"{show(old_work)} and {show(wroot)}; keep one, delete the other, and re-run (D-062, #1137)")
        for n in names:
            src, dst = os.path.join(old_work, n), os.path.join(wroot, n)
            _move(src, dst)
            moved.append((src, dst))
        os.rmdir(old_work)

    for n in ("context.md", "context-lock.json"):
        src = os.path.join(legacy, n)
        if os.path.isfile(src):
            dst = os.path.join(os.path.dirname(wroot), n)
            if os.path.lexists(dst):
                os.remove(src)      # a regenerable ad-hoc pack; the new location wins
                _say(f"dropped {show(src)} (superseded by {show(dst)})")
            else:
                _move(src, dst)
                moved.append((src, dst))

    old_cache = os.path.join(legacy, "cache")
    if os.path.isdir(old_cache):
        croot = _cache_root()
        for sub in sorted(os.listdir(old_cache)):
            s_dir = os.path.join(old_cache, sub)
            if not os.path.isdir(s_dir) or os.path.islink(s_dir):
                _remove(s_dir)      # stray file in the cache root: nothing reads it
                continue
            for name in sorted(os.listdir(s_dir)):
                src, dst = os.path.join(s_dir, name), os.path.join(croot, sub, name)
                with cache_lock(os.path.join(croot, sub), name):
                    if os.path.lexists(dst):
                        _remove(src)
                        _say(f"dropped {show(src)} (already cached at {dst})")
                        continue
                    _move(src, dst)
                moved.append((src, dst))
            os.rmdir(s_dir)
        os.rmdir(old_cache)

    try:
        os.rmdir(legacy)            # only when nothing else (product config) is left
    except OSError:
        pass
    for src, dst in moved:
        _say(f"moved {show(src)} -> {show(dst)} (D-062: tooling state left {LEGACY_DIR}/, #1137)")
    return moved


class cache_lock:
    """An exclusive advisory lock on `<directory>/.<name>.lock`, so concurrent worktrees never fetch or
    migrate the same upstream at once. Degrades to no lock where `fcntl` is unavailable."""

    def __init__(self, directory: str, name: str):
        self.path = os.path.join(directory, f".{name}.lock")
        self.fh = None

    def __enter__(self):
        os.makedirs(os.path.dirname(self.path), exist_ok=True)
        self.fh = open(self.path, "a")
        try:
            import fcntl
            fcntl.flock(self.fh.fileno(), fcntl.LOCK_EX)
        except ImportError:  # pragma: no cover - non-POSIX
            pass
        return self

    def __exit__(self, *exc):
        self.fh.close()             # closing the descriptor releases the flock
        return False


def active_issue() -> str | None:
    """The most-recently-touched work/<id> whose change.yaml exists."""
    base = work_root()
    if not os.path.isdir(base):
        return None
    candidates = []
    for name in os.listdir(base):
        cy = os.path.join(base, name, "change.yaml")
        if os.path.isfile(cy):
            candidates.append((os.path.getmtime(cy), name))
    if not candidates:
        return None
    candidates.sort(reverse=True)
    return candidates[0][1]


# ---- crate layout ---------------------------------------------------------------

# Layer names used by dependencies.yaml `allowed_layers` are the crate dir names.
CRATES = ["core"]  # collapsed to a two-crate editor (D-039 / RFC-0012); deferred boundaries return with their crate


def crate_of(relpath: str) -> str | None:
    parts = relpath.split("/")
    if len(parts) >= 2 and parts[0] == "crates":
        return parts[1]
    return None


@dataclass
class ChangeSet:
    """A resolved set of changed paths plus how it was obtained (for honest output)."""
    files: list[str] = field(default_factory=list)
    source: str = "none"      # git-base | git-head | explicit | change-yaml | none
    base: str | None = None

    @property
    def crates(self) -> list[str]:
        return sorted({c for f in self.files if (c := crate_of(f))})


def resolve_changeset(base: str | None = None,
                      files: list[str] | None = None) -> ChangeSet:
    """Precedence: explicit --files > git diff (base or HEAD) > empty."""
    if files:
        return ChangeSet(files=sorted({f.replace(os.sep, "/") for f in files}),
                         source="explicit")
    cf = changed_files(base)
    if cf is not None:
        return ChangeSet(files=cf, source=("git-base" if base else "git-head"), base=base)
    return ChangeSet(files=[], source="none", base=base)
