"""Repo location, the changed-file set, and the local .ruse/work layout.

The change tooling is git-aware but not git-required. `changed_files()` returns None
when this is not a git checkout (callers then fall back to an explicit --files list or
change.yaml's declared paths), so no command hard-crashes outside git.
"""
from __future__ import annotations

import os
import subprocess
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


# ---- .ruse/work layout ----------------------------------------------------------

def work_dir(issue: str) -> str:
    return path(".ruse", "work", str(issue))


def active_issue() -> str | None:
    """The most-recently-touched .ruse/work/<id> whose change.yaml exists."""
    base = path(".ruse", "work")
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
CRATES = ["core", "plugin-protocol", "render-model", "terminal-platform",
          "workspace", "workspace-runtime"]


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
