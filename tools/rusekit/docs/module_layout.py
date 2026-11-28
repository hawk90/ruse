"""Re-measure the counts that docs/contributing/module-layout.md states (#1689).

The doc writes down how the source tree is laid out and gives measured evidence for each rule. Those
counts went stale by hand, so `ruse docs check` recomputes them. A mismatch in the structure a rule names
(the L2 `x/mod.rs` set, the T3 directory list, the hub set, the T4 table) is an ERROR; a volatile tally
(files per convention, module directories per form, topic files per hub) only WARNS, so a PR that adds a
file does not fail CI. `ruse docs check --write` rewrites every stale number in place.

How a claim is found: each entry of CLAIMS is a regex whose named groups are keys of `measure()`.
Every match's groups are compared with the measured value. A regex that matches nothing is an error
too, so rewording a sentence cannot silently drop its check: update the regex with the prose.
The hub table must list exactly the measured hubs (its topic counts warn). The L2 bullet lists must name
exactly the `x/mod.rs` directories, and the T3 list exactly the directories with a beside-module suite.

Definitions (they match the shell in the doc's "Re-measuring" section):
  * module directory: a directory under `<crate>/src/` that holds `mod.rs` ("mod") or has a sibling
    `x.rs` ("x"); an "x" directory named `tests` or `*_tests` is test-only.
  * T1: a `src/` file with a line `mod tests {`.   T2: a `src/` file named `tests.rs`.
  * hub: a `tests.rs` / `*_tests.rs` with a same-named directory beside it.
  * T3: a `src/` file named `*_tests.rs`, "under a test directory" when its parent directory is named
    `tests` or `*_tests`, else "beside the module".
"""
from __future__ import annotations

import glob
import os
import re

DOC = "docs/contributing/module-layout.md"
CRATES = {"core": "crates/core", "tui": "apps/tui"}

_N = r"(?P<{}>\d+)"


def _c(pattern: str) -> re.Pattern:
    """Compile a claim: `{key}` is a measured number, a space matches any whitespace (line wraps)."""
    pattern = re.sub(r"\{(\w+)\}", lambda m: _N.format(m.group(1)), pattern)
    return re.compile(pattern.replace(" ", r"\s+"))


CLAIMS = [
    _c(r"the {mod_total} `x/mod\.rs` directories are older"),
    _c(r"\| {x_nontest} of {nontest_total} non-test module directories;"),
    _c(r"\| {mod_total} directories, listed below \|"),
    _c(r"\| T1 \|.*\| {t1_total} files \|"),
    _c(r"\| T2 \|.*\| {t2_total} files \({t2_hubs} of them T3 hubs\) \|"),
    _c(r"\| T3 \|.*\| {t3_beside} files beside the module, {t3_under} under a test directory \|"),
    _c(r"\| T4 \|.*\| all but {t4_exceptions} helper files \|"),
    _c(r"\| `crates/core` \| {x_nontest_core} \| {x_test_core} \(.*\) \| {mod_core} \|"),
    _c(r"\| `apps/tui` \| {x_nontest_tui} \| {x_test_tui} \(.*\) \| {mod_tui} \|"),
    _c(r"\| Total \| {x_nontest} \| {x_test} \| {mod_total} \|"),
    _c(r"These {mod_total} directories use `x/mod\.rs`"),
]
# Keys whose mismatch is an ERROR: they change only with the structure the rules name (the L2 set, the
# hub set, the T4 table). Every other key is a volatile tally and only WARNS (#1689 review).
STRUCTURAL = {"mod_total", "mod_core", "mod_tui", "t2_hubs", "t4_exceptions"}
# `| `crates/core/src/editor/tests.rs` + `editor/tests/` | 12 |`
HUB_ROW = re.compile(r"^[ \t]*\| `((?:crates|apps)/[\w/]+?)\.rs` \+ `[\w/]+/` \| (\d+) \|", re.M)
# `| `crates/core/src/editor/ex_cmd/samples.rs` | role |`: a row of the T4 exceptions table.
T4_ROW = re.compile(r"^\| `((?:crates|apps)/[\w/]+\.rs)` \| (?!\d)", re.M)
# "- `crates/core/src/`: `command/`, `editor/`, ..." under L2.
# "All in `apps/tui` (`app/dispatch/`, ...)": the directories holding the T3 files beside a module.
BESIDE_LIST = re.compile(r"[Aa]ll in `apps/tui` \(((?:`[\w/]+/`,?\s*)+)\)")
MOD_LIST = re.compile(r"^- `((?:crates|apps)/\w+/src/)`: ((?:`[\w/]+/`,?\s*)+)\.", re.M)


def _is_test_dir(name: str) -> bool:
    return name == "tests" or name.endswith("_tests")


def _files(root: str, pattern: str) -> list[str]:
    return sorted(glob.glob(os.path.join(root, pattern), recursive=True))


def measure(root: str) -> dict:
    """Every count the doc states, measured under `root` (the repo root)."""
    f: dict = {"mod_dirs": set(), "hub_topics": {}, "beside_dirs": set()}
    totals = {k: 0 for k in ("x_nontest", "x_test", "mod", "t1", "t2", "t3_beside", "t3_under",
                             "t2_hubs")}
    for short, crate in CRATES.items():
        src = os.path.join(root, crate, "src")
        counts = {"x_nontest": 0, "x_test": 0, "mod": 0}
        for d, _, _ in os.walk(src):
            if d == src:
                continue
            if os.path.isfile(os.path.join(d, "mod.rs")):
                counts["mod"] += 1
                f["mod_dirs"].add(_rel(d, root) + "/")
            elif os.path.isfile(d + ".rs"):
                counts["x_test" if _is_test_dir(os.path.basename(d)) else "x_nontest"] += 1
        rs = _files(src, "**/*.rs")
        t1 = sum(1 for p in rs if re.search(r"^\s*mod tests\s*\{", _read(p), re.M))
        t2 = [p for p in rs if os.path.basename(p) == "tests.rs"]
        t3 = [p for p in rs if p.endswith("_tests.rs")]
        for p in t2 + t3:
            hub = p[:-3]
            if os.path.isdir(hub):
                f["hub_topics"][_rel(hub, root)] = len(
                    _files(hub, "*_tests.rs"))
                totals["t2_hubs"] += p in t2
        beside = [p for p in t3 if not _is_test_dir(os.path.basename(os.path.dirname(p)))]
        under = len(t3) - len(beside)
        f["beside_dirs"] |= {(crate, _rel(os.path.dirname(p), src) + "/") for p in beside}
        f.update({f"x_nontest_{short}": counts["x_nontest"], f"x_test_{short}": counts["x_test"],
                  f"mod_{short}": counts["mod"], f"t1_{short}": t1, f"t2_{short}": len(t2)})
        for k in ("x_nontest", "x_test", "mod"):
            totals[k] += counts[k]
        totals["t1"] += t1
        totals["t2"] += len(t2)
        totals["t3_beside"] += len(t3) - under
        totals["t3_under"] += under
    f.update({"x_nontest": totals["x_nontest"], "x_test": totals["x_test"],
              "mod_total": totals["mod"], "nontest_total": totals["x_nontest"] + totals["mod"],
              "t1_total": totals["t1"], "t2_total": totals["t2"], "t2_hubs": totals["t2_hubs"],
              "t3_beside": totals["t3_beside"], "t3_under": totals["t3_under"]})
    return f


def _rel(path: str, start: str) -> str:
    return os.path.relpath(path, start).replace(os.sep, "/")


def _read(path: str) -> str:
    with open(path, encoding="utf-8") as fh:
        return fh.read()


def check(text: str, root: str, facts: dict | None = None) -> tuple[list[str], list[str], str]:
    """(errors, warnings, fixed text). The fixed text has every stale number replaced by its
    measurement; a claim that matches nothing or a wrong directory list cannot be fixed."""
    facts = dict(facts if facts is not None else measure(root))
    facts["t4_exceptions"] = len(T4_ROW.findall(text))  # "all but N helper files" = the T4 rows
    errors: list[str] = []
    warns: list[str] = []
    edits: list[tuple[int, int, str]] = []   # (start, end, new) in `text`

    def lineno(pos: int) -> int:
        return text.count("\n", 0, pos) + 1

    def compare(m: re.Match, group, want: int, what: str, structural: bool) -> None:
        got = int(m.group(group))
        if got != want:
            msg = f"{DOC}:{lineno(m.start(group))}: {what} says {got}, measured {want}"
            if structural:
                errors.append(msg)
            else:
                warns.append(msg + " (`ruse docs check --write` updates it)")
            edits.append((m.start(group), m.end(group), str(want)))

    for rx in CLAIMS:
        matches = list(rx.finditer(text))
        if not matches:
            errors.append(f"{DOC}: no text matches the count claim /{rx.pattern}/ — reworded? "
                          f"update CLAIMS in tools/rusekit/docs/module_layout.py")
        for m in matches:
            for key in m.groupdict():
                compare(m, key, facts[key], key, key in STRUCTURAL)
    for rx, what in ((HUB_ROW, "T3 hub table row"), (MOD_LIST, "L2 directory list"),
                     (BESIDE_LIST, "T3 'All in `apps/tui` (...)' directory list")):
        if not rx.search(text):
            errors.append(f"{DOC}: no {what} found — reworded? update module_layout.py")

    hubs = set()
    for m in HUB_ROW.finditer(text):
        hubs.add(m.group(1))
        if m.group(1) in facts["hub_topics"]:
            compare(m, 2, facts["hub_topics"][m.group(1)], f"topic files of hub {m.group(1)}", False)
    for h in sorted(set(facts["hub_topics"]) - hubs):
        errors.append(f"{DOC}: the T3 hub table does not list the test hub {h}.rs + {h}/")
    for h in sorted(hubs - set(facts["hub_topics"])):
        errors.append(f"{DOC}: the T3 hub table lists {h}.rs, which is not a test hub")

    listed: set[str] = set()
    for m in MOD_LIST.finditer(text):
        listed |= {m.group(1) + d for d in re.findall(r"`([\w/]+/)`", m.group(2))}
    for d in sorted(facts["mod_dirs"] - listed):
        errors.append(f"{DOC}: L2 does not list the `x/mod.rs` directory {d}")
    for d in sorted(listed - facts["mod_dirs"]):
        errors.append(f"{DOC}: L2 lists {d}, which has no mod.rs")

    beside = {("apps/tui", d) for m in BESIDE_LIST.finditer(text)
              for d in re.findall(r"`([\w/]+/)`", m.group(1))}
    for crate, d in sorted(facts["beside_dirs"] ^ beside):
        where = "has no" if (crate, d) in beside else "is missing from the list of"
        errors.append(f"{DOC}: T3: {crate}/src/{d} {where} `<topic>_tests.rs` beside a module")

    fixed = text
    for start, end, new in sorted(set(edits), reverse=True):
        fixed = fixed[:start] + new + fixed[end:]
    return errors, warns, fixed


def run(root: str, write: bool = False) -> tuple[list[str], list[str]]:
    """Check (and with `write`, fix) the doc under `root`; returns the (errors, warnings) left."""
    path = os.path.join(root, DOC)
    text = _read(path)
    facts = measure(root)
    errors, warns, fixed = check(text, root, facts)
    if write and fixed != text:
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(fixed)
        errors, warns, _ = check(fixed, root, facts)
    return errors, warns
