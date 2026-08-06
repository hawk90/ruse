#!/usr/bin/env python3
"""
gen_roadmap — generate the delivery-phase table inside docs/parity/roadmap.md from spec/phases.yaml.

Source of truth is spec/phases.yaml; roadmap.md is narrative PLUS this one generated table (between the
BEGIN/END markers). Keeps the phase→feature mapping from drifting into hand-written prose.

Usage:
  python3 tools/gen_roadmap.py            # --check: fail (exit 1) if the table is stale
  python3 tools/gen_roadmap.py --write    # regenerate the table in place
"""
import sys, re, argparse

try:
    import yaml
except ImportError:
    print("FAIL: PyYAML not installed"); sys.exit(1)

PHASES = "spec/phases.yaml"
ROADMAP = "docs/parity/roadmap.md"
BEGIN = "<!-- BEGIN GENERATED phases: spec/phases.yaml (regenerate: python3 tools/gen_roadmap.py --write) -->"
END = "<!-- END GENERATED phases -->"


def _oneline(s):
    return " ".join((s or "").split())


def table(path=PHASES):
    cat = yaml.safe_load(open(path))
    rows = ["| # | Phase | Stage | Features | Goal |", "| --- | --- | --- | --- | --- |"]
    for i, ph in enumerate(cat.get("phases") or [], 1):
        feats = ", ".join(ph.get("includes") or [])
        rows.append(f"| {i} | `{ph['id']}` | {ph.get('stage')} | {feats} | {_oneline(ph.get('goal'))} |")
    return "\n".join(rows)


def render_block():
    return f"{BEGIN}\n{table()}\n{END}"


def _current(text):
    m = re.search(re.escape(BEGIN) + r".*?" + re.escape(END), text, re.S)
    return m.group(0) if m else None


def main(argv):
    ap = argparse.ArgumentParser(prog="gen_roadmap")
    ap.add_argument("--write", action="store_true", help="rewrite the table in place (default is --check)")
    args = ap.parse_args(argv)

    text = open(ROADMAP).read()
    cur = _current(text)
    if cur is None:
        print(f"FAIL: markers not found in {ROADMAP} — add the BEGIN/END phase markers first")
        return 1
    fresh = render_block()

    if args.write:
        if cur != fresh:
            open(ROADMAP, "w").write(text.replace(cur, fresh))
            print(f"gen_roadmap: rewrote phase table in {ROADMAP}")
        else:
            print("gen_roadmap: already current")
        return 0

    if cur != fresh:
        print(f"FAIL: phase table in {ROADMAP} is stale — run `python3 tools/gen_roadmap.py --write`")
        return 1
    print("gen_roadmap: phase table is current")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
