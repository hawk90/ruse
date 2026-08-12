#!/usr/bin/env python3
"""gov parity_oracle — the fixture corpora must be TRUSTWORTHY evidence (guards the parity oracle axis).

`parity_discovery` proves each upstream CENSUS is answerable to its pin. This checker closes the sibling
edge on the BEHAVIOURAL half of parity: a fixture corpus is a set of hand-verified claims about what an
upstream editor's language does, and each claim is only evidence if its `expect` was captured from the
PINNED editor — never hand-written, never captured through the wrong binary
(spec/parity/upstreams.yaml#oracle_selftest). A fixture with an empty `expect`, or a corpus that records a
version other than the pin, is not weak evidence — it is a LIE wearing a number, exactly the failure class
parity_discovery exists to make impossible one level up.

This gate is PER ORACLE AXIS (see AXES): the Neovim corpus (tests/parity/vim/fixtures/corpus.yaml, captured
by tools/parity/oracle.py) and the Emacs corpus (tests/parity/emacs/fixtures/corpus.yaml, captured by
tools/parity/emacs_oracle.py via call-interactively). Each axis observes different things — Neovim records
{cursor, reg_unnamed}, Emacs records {point, mark, kill} — so the integrity check is parameterized per axis
rather than assuming one observable shape. A corpus that is absent (its axis not yet generated) is SKIPPED,
not failed: a missing corpus is "no evidence yet", which is honest; a PRESENT corpus must be trustworthy.

Two tiers per axis, because CI has no editor binaries and this gate must stay green there:

  INTEGRITY (always, no binary needed)
      Every fixture carries a non-empty oracle-captured `expect` (text a non-empty line list, plus the
      axis's observable keys present), and the corpus records the editor version + revision it was
      generated against, matching that editor's pin in upstreams.yaml. FAILS on a fixture with no expect,
      or a recorded version/revision != pin. Checkable with nothing installed, so it runs everywhere.

  SELFTEST (only when the editor binary is on PATH AND == the pin)
      Runs `<oracle> --selftest` — the harness's own non-corruption proof. FAILS if the selftest fails (a
      harness that corrupts observation invalidates every fixture recorded through it). When the binary is
      absent or is not the pinned version, the selftest is SKIPPED with a WARN and the axis PASSES: the
      integrity tier already caught a corpus captured against the wrong binary, and re-proving
      non-corruption needs the actual pinned binary, which CI does not have.

Auto-discovered into `ruse gov check`.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys

import yaml

from rusekit import render, repo  # noqa: E402

UPSTREAMS = "spec/parity/upstreams.yaml"

# One entry per oracle axis. Each names the pinned editor, its corpus, its harness, and — because the two
# oracles observe DIFFERENT state — the observable keys its `expect` must carry and the `oracle.<field>`
# that records the binary version at capture time. `match_token` turns a pin's version_label into the
# token expected in `<binary> --version`: Neovim's label IS that token ("v0.12.4" in "NVIM v0.12.4");
# Emacs's label is "emacs-30.2" but `emacs --version` says "GNU Emacs 30.2", so the bare number is matched.
AXES = [
    {
        "editor": "neovim",
        "pin_key": "neovim",
        "corpus": "tests/parity/vim/fixtures/corpus.yaml",
        "oracle": "tools/parity/oracle.py",
        "binary": "nvim",
        "version_argv": ["nvim", "--version"],
        "oracle_version_field": "nvim_version",
        "expect_keys": ["cursor", "reg_unnamed"],
        "dict_keys": ["reg_unnamed"],  # keys whose value must be a dict (a captured {text,type})
        "match_token": lambda label: label or "",
    },
    {
        "editor": "emacs",
        "pin_key": "emacs",
        "corpus": "tests/parity/emacs/fixtures/corpus.yaml",
        "oracle": "tools/parity/emacs_oracle.py",
        "binary": "emacs",
        "version_argv": ["emacs", "--version"],
        "oracle_version_field": "emacs_version",
        # point/mark/kill are the compared observables; mark/kill are legitimately null (key present,
        # value None) so only key-presence is required here, not a non-null value.
        "expect_keys": ["point", "mark", "kill"],
        "dict_keys": [],
        "match_token": lambda label: re.sub(r"^emacs-", "", label or ""),
    },
]


def load_pin(pin_key: str) -> dict:
    """The editor's pin (version_label + revision) from the read-only evidence layer."""
    with open(repo.path(UPSTREAMS), encoding="utf-8") as fh:
        doc = yaml.safe_load(fh) or {}
    ed = ((doc.get("upstreams") or {}).get(pin_key)) or {}
    return {"version_label": ed.get("version_label"), "revision": ed.get("revision")}


def load_corpus(path: str) -> dict | None:
    """The corpus is JSON-in-YAML (a subset of YAML 1.2); json.loads is the strict, dependency-free reader.

    Returns None when the corpus file does not exist yet — a not-yet-generated axis is "no evidence", which
    is skipped (honest), not failed.
    """
    full = repo.path(path)
    if not os.path.exists(full):
        return None
    with open(full, encoding="utf-8") as fh:
        return json.loads(fh.read())


def _version_matches(version_line: str, token: str) -> bool:
    """True when `<binary> --version`'s first line names `token` as a whole word (assert_pin's rule)."""
    return bool(token) and bool(re.search(rf"\b{re.escape(token)}\b", version_line or ""))


def local_version(argv: list[str]) -> str | None:
    """First line of `<binary> --version`, or None when the binary is not on PATH (the CI case)."""
    try:
        out = subprocess.run(argv, capture_output=True, text=True, check=True)
    except (OSError, subprocess.CalledProcessError):
        return None
    lines = out.stdout.splitlines()
    return lines[0].strip() if lines else None


def check_integrity(corpus: dict, pin: dict, axis: dict) -> dict:
    """The always-on tier: fixtures have real captured expects, and the corpus is answerable to the pin."""
    oracle = corpus.get("oracle") or {}
    fixtures = corpus.get("fixtures") or []
    token = axis["match_token"](pin["version_label"])

    no_expect: list[str] = []
    for fx in fixtures:
        name = fx.get("name", "<unnamed>")
        expect = fx.get("expect")
        if not isinstance(expect, dict):
            no_expect.append(name)
            continue
        text = expect.get("text")
        # `text` must be a real line list (a deletion legitimately yields [""], a non-empty list — an
        # ABSENT/empty list is not), and every axis observable key must be present. Keys named in
        # `dict_keys` additionally must carry a dict (a captured {text,type} sub-record).
        ok = isinstance(text, list) and len(text) >= 1
        for k in axis["expect_keys"]:
            ok = ok and (k in expect)
        for k in axis["dict_keys"]:
            ok = ok and isinstance(expect.get(k), dict)
        if not ok:
            no_expect.append(name)

    # Version/revision the corpus was captured against, matched to the read-only pin. A corpus generated
    # through the wrong binary records the wrong provenance here — the "fixture captured against the wrong
    # editor is a lie" case.
    pin_mismatch: list[tuple[str, str, str]] = []
    got_label = oracle.get("pin_version_label")
    got_rev = oracle.get("pin_revision")
    got_binary = oracle.get(axis["oracle_version_field"])
    if got_label != pin["version_label"]:
        pin_mismatch.append(("pin_version_label", str(got_label), str(pin["version_label"])))
    if got_rev != pin["revision"]:
        pin_mismatch.append(("pin_revision", str(got_rev), str(pin["revision"])))
    # The binary actually used at capture time must have been the pinned release, not merely labelled so.
    if not _version_matches(got_binary or "", token):
        pin_mismatch.append((axis["oracle_version_field"], str(got_binary), str(pin["version_label"])))

    return {
        "fixtures": len(fixtures),
        "no_expect": no_expect,
        "pin_mismatch": pin_mismatch,
    }


def run_selftest(oracle_path: str) -> int:
    """Run the harness's own non-corruption gate. Returns the oracle's exit code."""
    proc = subprocess.run(
        [sys.executable, repo.path(oracle_path), "--selftest"],
        capture_output=True,
        text=True,
    )
    return proc.returncode


def check_axis(axis: dict) -> dict:
    """Run both tiers for one oracle axis. Returns a per-axis result summary (status='skipped' when the
    corpus is absent — a not-yet-generated axis is honest 'no evidence', not a failure)."""
    pin = load_pin(axis["pin_key"])
    corpus = load_corpus(axis["corpus"])
    if corpus is None:
        return {"axis": axis, "pin": pin, "absent": True}

    integrity = check_integrity(corpus, pin, axis)
    version_line = local_version(axis["version_argv"])
    token = axis["match_token"](pin["version_label"])
    binary_ok = version_line is not None and _version_matches(version_line, token)
    selftest_status = "skipped"
    selftest_failed = False
    if binary_ok:
        selftest_status = "ran"
        selftest_failed = run_selftest(axis["oracle"]) != 0

    return {
        "axis": axis,
        "pin": pin,
        "absent": False,
        "integrity": integrity,
        "version_line": version_line,
        "binary_ok": binary_ok,
        "selftest_status": selftest_status,
        "selftest_failed": selftest_failed,
    }


def _render_axis(r: dict) -> bool:
    """Render one axis's result; return True if it FAILED."""
    axis, pin = r["axis"], r["pin"]
    editor, binary = axis["editor"], axis["binary"]
    render.heading(f"gov parity_oracle · {editor} (fixture corpus is trustworthy evidence)")

    if r["absent"]:
        render.field("Corpus", f"{axis['corpus']} — absent (axis not generated yet)")
        render.warn(f"{editor} corpus not present — SKIPPED (a not-yet-generated axis is no evidence, "
                    f"not a failure; generate with `python3 {axis['oracle']} --generate`)")
        return False

    integ = r["integrity"]
    render.field("Fixtures", str(integ["fixtures"]))
    render.field(f"Pin ({editor})", f"{pin['version_label']} / {str(pin['revision'])[:12]}")
    render.field("Integrity", "checked (no binary needed)")
    if r["binary_ok"]:
        render.field("Selftest", f"ran — local {binary} {r['version_line']} == pin")
    elif r["version_line"] is None:
        render.field("Selftest", f"skipped — {binary} not present")
    else:
        render.field("Selftest", f"skipped — local {binary} {r['version_line']} != pin {pin['version_label']}")

    for name in integ["no_expect"]:
        render.bullet(f"{name}: no oracle-captured `expect` ({'/'.join(['text'] + axis['expect_keys'])}) — "
                      f"a fixture without a captured expect is a claim with no evidence; regenerate with "
                      f"`python3 {axis['oracle']} --generate`", mark="!")
    for field, got, want in integ["pin_mismatch"]:
        render.bullet(f"corpus oracle.{field} is `{got}` but the pin is `{want}` — the corpus was captured "
                      f"against the wrong {editor}; a fixture recorded through the wrong binary is not "
                      f"evidence. Regenerate against the pinned {binary}", mark="!")
    if r["selftest_failed"]:
        render.bullet(f"{axis['oracle']} --selftest FAILED — the harness is corrupting its own observation, "
                      f"so every fixture recorded through it is untrustworthy (run it directly to see which "
                      f"case)", mark="!")

    failed = bool(integ["no_expect"] or integ["pin_mismatch"] or r["selftest_failed"])
    if failed:
        render.fail(f"{editor} corpus is not trustworthy evidence "
                    f"({len(integ['no_expect'])} missing-expect, {len(integ['pin_mismatch'])} pin-mismatch, "
                    f"selftest {'FAILED' if r['selftest_failed'] else r['selftest_status']})")
        return True

    if r["selftest_status"] == "skipped":
        render.warn(f"{editor} oracle selftest skipped — {binary} not present / not pinned; corpus "
                    f"INTEGRITY still verified against the pin (this keeps `gov check` green in CI, which "
                    f"has no {binary})")
    render.ok(f"{integ['fixtures']} {editor} fixtures carry oracle-captured expects at pin "
              f"{pin['version_label']}; selftest {r['selftest_status']}")
    return False


def main(argv=None) -> int:
    argparse.ArgumentParser(prog="gov parity_oracle").parse_args(argv or [])

    any_failed = False
    any_present = False
    for axis in AXES:
        r = check_axis(axis)
        any_present = any_present or not r["absent"]
        any_failed = _render_axis(r) or any_failed

    if any_failed:
        return 1
    if not any_present:
        render.warn("no fixture corpus present on any oracle axis — nothing to gate yet")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
