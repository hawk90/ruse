#!/usr/bin/env python3
"""Print the pinned oracle binary of one editor as shell `KEY=value` lines (#904).

The scheduled recapture (.github/workflows/oracle.yml) installs exactly what spec/parity/upstreams.yaml pins:
`oracles.<editor>.install_url` and `install_sha256`, plus the editor's `upstreams.<editor>.version_label`.
The URL must name that version, so a bumped label with a stale URL (or the reverse) fails here, not after a
download.

Stdlib only, like the oracles (D-034): the file is read by indentation, as oracle.py's `read_pin` does.

Usage:
    python3 tools/parity/install_pin.py neovim > pin.env && . ./pin.env   # ORACLE_URL, _SHA256, _VERSION
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

UPSTREAMS = Path(__file__).resolve().parents[2] / "spec" / "parity" / "upstreams.yaml"


def block(lines: list[str], section: str, name: str) -> dict[str, str]:
    """The scalar `key: value` fields of `<section>.<name>` (two-space key, four-space fields)."""
    try:
        start = lines.index(f"{section}:")
        at = lines.index(f"  {name}:", start + 1)
    except ValueError as exc:
        raise SystemExit(f"no {section}.{name} in {UPSTREAMS}") from exc
    out: dict[str, str] = {}
    for ln in lines[at + 1 :]:
        if re.match(r"^ {0,2}\S", ln):
            break
        m = re.match(r"^    ([a-z0-9_]+): *([^#]*?) *(?:#.*)?$", ln)
        if m and m.group(2):
            out[m.group(1)] = m.group(2).strip('"')
    return out


def pin(editor: str) -> dict[str, str]:
    lines = UPSTREAMS.read_text(encoding="utf-8").splitlines()
    oracle = block(lines, "oracles", editor)
    label = block(lines, "upstreams", editor).get("version_label", "")
    url, sha = oracle.get("install_url", ""), oracle.get("install_sha256", "")
    if not (url and re.fullmatch(r"[0-9a-f]{64}", sha) and label):
        raise SystemExit(f"oracles.{editor} needs install_url and a sha256, upstreams.{editor} a version_label")
    version = label.removeprefix("emacs-")
    if f"{version}/" not in url and f"-{version}." not in url:
        raise SystemExit(f"oracles.{editor}.install_url {url} does not name version_label {label}")
    return {"ORACLE_URL": url, "ORACLE_SHA256": sha, "ORACLE_VERSION": version}


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print(__doc__)
        return 2
    for key, value in pin(argv[1]).items():
        print(f"{key}={value}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
