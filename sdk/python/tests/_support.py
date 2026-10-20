"""Shared test helpers: puts the package on sys.path and names the fake incode."""

import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
PACKAGE_ROOT = os.path.dirname(HERE)
if PACKAGE_ROOT not in sys.path:
    sys.path.insert(0, PACKAGE_ROOT)

FAKE_INCODE = os.path.join(os.path.dirname(PACKAGE_ROOT), "testing", "fake_incode.py")
# Run through this interpreter, so the tests do not depend on the script's
# executable bit or on a `python3` on PATH.
FAKE = [sys.executable, FAKE_INCODE]
