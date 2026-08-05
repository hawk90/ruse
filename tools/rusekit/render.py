"""Consistent terminal output for every ruse subcommand.

Color is opt-in on a TTY and disabled by NO_COLOR / non-tty / RUSE_NO_COLOR so output
stays clean in CI logs and PR bodies.
"""
from __future__ import annotations

import os
import sys

_USE_COLOR = (
    sys.stdout.isatty()
    and os.environ.get("NO_COLOR") is None
    and os.environ.get("RUSE_NO_COLOR") is None
)

_C = {
    "reset": "\033[0m", "bold": "\033[1m", "dim": "\033[2m",
    "red": "\033[31m", "green": "\033[32m", "yellow": "\033[33m",
    "blue": "\033[34m", "cyan": "\033[36m",
}


def c(text: str, color: str) -> str:
    if not _USE_COLOR:
        return text
    return f"{_C.get(color, '')}{text}{_C['reset']}"


def heading(text: str) -> None:
    print(c(text, "bold"))


def field(label: str, value: str, width: int = 22) -> None:
    print(f"{label + ':':<{width}}{value}")


def bullet(text: str, mark: str = "-") -> None:
    print(f"  {mark} {text}")


def ok(text: str) -> None:
    print(c(f"PASS", "green") + f" {text}" if text else c("PASS", "green"))


def fail(text: str) -> None:
    print(c(f"FAIL", "red") + f" {text}" if text else c("FAIL", "red"))


def warn(text: str) -> None:
    print(c("WARN", "yellow") + f" {text}")


def result(passed: bool, summary: str = "") -> None:
    (ok if passed else fail)(summary)


def tree(lines: list[tuple[int, str]]) -> None:
    """Render (depth, label) pairs as an indented tree."""
    for depth, label in lines:
        if depth == 0:
            print(label)
        else:
            print("  " * (depth - 1) + " └─ " + label)


def kv_block(pairs: list[tuple[str, str]], width: int = 22) -> None:
    for k, v in pairs:
        field(k, v, width)
