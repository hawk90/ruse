#!/usr/bin/env python3
"""gov parity_oracle — the fixture corpus must be TRUSTWORTHY evidence (guards the parity oracle axis).

`parity_discovery` proves the upstream CENSUS is answerable to its pin. This checker closes the sibling
edge on the BEHAVIOURAL half of parity: the Neovim fixture corpus (tests/parity/vim/fixtures/corpus.yaml)
is a set of hand-verified claims about what Vim's editing language does, and each claim is only evidence
if its `expect` was captured from the PINNED Neovim — never hand-written, never captured through the wrong
binary (spec/parity/upstreams.yaml#oracle_selftest). A fixture with an empty `expect`, or a corpus that
records a version other than the pin, is not weak evidence — it is a LIE wearing a number, exactly the
failure class parity_discovery exists to make impossible one level up.

Two tiers, because CI has no Neovim and this gate must stay green there:

  INTEGRITY (always, no binary needed)
      Every fixture carries a non-empty oracle-captured `expect` (text/cursor/reg_unnamed present, text a
      non-empty line list), and the corpus records the nvim version + revision it was generated against,
      matching the neovim pin in upstreams.yaml. FAILS on a fixture with no expect, or a recorded
      version/revision != pin. This is checkable with nothing installed, so it runs everywhere.

  SELFTEST (only when `nvim` is on PATH AND == the pin)
      Runs `tools/parity/oracle.py --selftest` — the harness's own non-corruption proof. FAILS if the
      selftest fails (a harness that corrupts observation invalidates every fixture recorded through it).
      When nvim is absent or is not the pinned version, the selftest is SKIPPED with a WARN and the gate
      PASSES: the integrity tier already caught a corpus captured against the wrong binary, and re-proving
      non-corruption needs the actual pinned binary, which CI does not have.

Auto-discovered into `ruse gov check`.
"""
from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys

import yaml

from rusekit import render, repo  # noqa: E402

CORPUS = "tests/parity/vim/fixtures/corpus.yaml"
UPSTREAMS = "spec/parity/upstreams.yaml"
ORACLE = "tools/parity/oracle.py"


def load_pin() -> dict:
    """The neovim pin (version_label + revision) from the read-only evidence layer."""
    with open(repo.path(UPSTREAMS), encoding="utf-8") as fh:
        doc = yaml.safe_load(fh) or {}
    nv = ((doc.get("upstreams") or {}).get("neovim")) or {}
    return {"version_label": nv.get("version_label"), "revision": nv.get("revision")}


def load_corpus() -> dict:
    """The corpus is JSON-in-YAML (a subset of YAML 1.2); json.loads is the strict, dependency-free reader."""
    with open(repo.path(CORPUS), encoding="utf-8") as fh:
        return json.loads(fh.read())


def _version_matches(version_line: str, label: str) -> bool:
    """True when `nvim --version`'s first line names the pinned version_label as a token (assert_pin's rule)."""
    return bool(label) and bool(re.search(rf"\b{re.escape(label)}\b", version_line or ""))


def local_nvim_version() -> str | None:
    """First line of `nvim --version`, or None when nvim is not on PATH (the CI case)."""
    try:
        out = subprocess.run(
            ["nvim", "--version"], capture_output=True, text=True, check=True
        )
    except (OSError, subprocess.CalledProcessError):
        return None
    lines = out.stdout.splitlines()
    return lines[0].strip() if lines else None


def check_integrity(corpus: dict, pin: dict) -> dict:
    """The always-on tier: fixtures have real captured expects, and the corpus is answerable to the pin."""
    oracle = corpus.get("oracle") or {}
    fixtures = corpus.get("fixtures") or []

    no_expect: list[str] = []
    for fx in fixtures:
        name = fx.get("name", "<unnamed>")
        expect = fx.get("expect")
        if not isinstance(expect, dict):
            no_expect.append(name)
            continue
        text = expect.get("text")
        # A captured expect names all three compared observables; `text` must be a real line list (a
        # deletion legitimately yields [""], which is still a non-empty list — an ABSENT/empty list is not).
        ok = (
            isinstance(text, list)
            and len(text) >= 1
            and "cursor" in expect
            and isinstance(expect.get("reg_unnamed"), dict)
        )
        if not ok:
            no_expect.append(name)

    # Version/revision the corpus was captured against, matched to the read-only pin. A corpus generated
    # through the wrong binary records the wrong provenance here — the "fixture captured against the wrong
    # nvim is a lie" case.
    pin_mismatch: list[tuple[str, str, str]] = []
    got_label = oracle.get("pin_version_label")
    got_rev = oracle.get("pin_revision")
    got_binary = oracle.get("nvim_version")
    if got_label != pin["version_label"]:
        pin_mismatch.append(("pin_version_label", str(got_label), str(pin["version_label"])))
    if got_rev != pin["revision"]:
        pin_mismatch.append(("pin_revision", str(got_rev), str(pin["revision"])))
    # The binary actually used at capture time must have been the pinned release, not merely labelled so.
    if not _version_matches(got_binary or "", pin["version_label"] or ""):
        pin_mismatch.append(("nvim_version", str(got_binary), str(pin["version_label"])))

    return {
        "fixtures": len(fixtures),
        "no_expect": no_expect,
        "pin_mismatch": pin_mismatch,
    }


def run_selftest() -> int:
    """Run the harness's own non-corruption gate. Returns the oracle's exit code."""
    proc = subprocess.run(
        [sys.executable, repo.path(ORACLE), "--selftest"],
        capture_output=True,
        text=True,
    )
    return proc.returncode


def main(argv=None) -> int:
    argparse.ArgumentParser(prog="gov parity_oracle").parse_args(argv or [])

    pin = load_pin()
    corpus = load_corpus()
    r = check_integrity(corpus, pin)

    # Selftest tier: only meaningful with the actual pinned binary present.
    version_line = local_nvim_version()
    nvim_ok = version_line is not None and _version_matches(version_line, pin["version_label"] or "")
    selftest_status = "skipped"
    selftest_failed = False
    if nvim_ok:
        selftest_status = "ran"
        selftest_failed = run_selftest() != 0

    render.heading("gov parity_oracle (fixture corpus is trustworthy evidence)")
    render.field("Fixtures", str(r["fixtures"]))
    render.field("Pin (neovim)", f"{pin['version_label']} / {str(pin['revision'])[:12]}")
    render.field("Integrity", "checked (no binary needed)")
    if nvim_ok:
        render.field("Selftest", f"ran — local nvim {version_line} == pin")
    elif version_line is None:
        render.field("Selftest", "skipped — nvim not present")
    else:
        render.field("Selftest", f"skipped — local nvim {version_line} != pin {pin['version_label']}")

    for name in r["no_expect"]:
        render.bullet(f"{name}: no oracle-captured `expect` (text/cursor/reg_unnamed) — a fixture without "
                      f"a captured expect is a claim with no evidence; regenerate with "
                      f"`python3 tools/parity/oracle.py --generate`", mark="!")
    for field, got, want in r["pin_mismatch"]:
        render.bullet(f"corpus oracle.{field} is `{got}` but the pin is `{want}` — the corpus was captured "
                      f"against the wrong Neovim; a fixture recorded through the wrong binary is not "
                      f"evidence. Regenerate against the pinned nvim", mark="!")
    if selftest_failed:
        render.bullet("oracle --selftest FAILED — the harness is corrupting its own observation, so every "
                      "fixture recorded through it is untrustworthy (run it directly to see which case)",
                      mark="!")

    if r["no_expect"] or r["pin_mismatch"] or selftest_failed:
        render.fail(f"corpus is not trustworthy evidence "
                    f"({len(r['no_expect'])} missing-expect, {len(r['pin_mismatch'])} pin-mismatch, "
                    f"selftest {'FAILED' if selftest_failed else selftest_status})")
        return 1

    if selftest_status == "skipped":
        render.warn("oracle selftest skipped — nvim not present / not pinned; corpus INTEGRITY still "
                    "verified against the pin (this keeps `gov check` green in CI, which has no nvim)")
    render.ok(f"{r['fixtures']} fixtures carry oracle-captured expects at pin {pin['version_label']}; "
              f"selftest {selftest_status}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
