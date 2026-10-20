"""Against the real binary: set INCODE_BIN=/path/to/incode to run; skipped otherwise.

Only `initialize` and `session/new` are exercised — no turn is run, so no agent
CLI or subscription is needed.
"""

import os
import tempfile
import unittest

import _support  # noqa: F401  (puts the package on sys.path)

from incode_sdk import Session

INCODE_BIN = os.environ.get("INCODE_BIN")


@unittest.skipUnless(INCODE_BIN, "set INCODE_BIN to run the live test against a real incode")
class LiveAcpTest(unittest.TestCase):
    def test_initialize_and_session_new(self):
        with tempfile.TemporaryDirectory() as ws:
            with Session(ws, incode_path=INCODE_BIN, request_timeout=60) as s:
                self.assertEqual(s.protocol_version, 1)
                self.assertEqual(s.agent_info.get("name"), "incode")
                self.assertTrue(s.session_id)
                self.assertIn("workspace-write", [m["id"] for m in s.available_modes])
                self.assertTrue(s.current_mode)
                self.assertTrue(any(o.get("id") == "agent" for o in s.config_options))


if __name__ == "__main__":
    unittest.main()
