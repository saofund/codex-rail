#!/usr/bin/env python3
"""Compatibility entry point for Rail's end-to-end regression suite.

The historical script duplicated a subset of cockpit.py and consequently
drifted to the removed e/w/s/Space shortcuts. Keep one executable source of
truth: callers of ``tests/regress.py`` now run the same sealed-interface audit
as CI, including guardian, prompt, process-tree, distill, and visual checks.

Usage: python3 tests/regress.py [./target/release/rail] [--png OUTDIR]
"""

import os
import sys

from cockpit import audit, visual_audit


HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)


def main():
    rail = (os.path.abspath(sys.argv[1])
            if len(sys.argv) > 1 and not sys.argv[1].startswith("-")
            else os.path.join(REPO, "target", "release", "rail"))
    pngdir = None
    if "--png" in sys.argv:
        pngdir = sys.argv[sys.argv.index("--png") + 1]
    runner = visual_audit if "--visual-only" in sys.argv else audit
    return 0 if runner(rail, pngdir) else 1


if __name__ == "__main__":
    raise SystemExit(main())
