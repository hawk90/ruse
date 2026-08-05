"""Shared: put tools/ on sys.path so tests import rusekit/change/docs/... like ruse.py does."""
import os
import sys

TOOLS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
if TOOLS not in sys.path:
    sys.path.insert(0, TOOLS)
