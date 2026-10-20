import asyncio
import json
import os
import tempfile
import unittest

from _support import FAKE

from incode_sdk import IncodeError, IncodeTimeout, ProtocolError, TaskFailed, TaskResult, run_task
from incode_sdk.aio import run_task_async
from incode_sdk.task import build_task_argv


class RunTaskTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.ws = self.tmp.name

    def tearDown(self):
        self.tmp.cleanup()

    def test_success_is_parsed(self):
        argv_file = os.path.join(self.ws, "argv.json")
        result = run_task(
            "add a test",
            workspace=self.ws,
            model="sonnet",
            permission_mode="read-only",
            incode_path=FAKE,
            env={"FAKE_INCODE_ARGV": argv_file},
        )
        self.assertIsInstance(result, TaskResult)
        self.assertFalse(result.is_error)
        self.assertEqual(result.result, "done: add a test")
        self.assertIsNone(result.error)
        self.assertEqual(result.model, "sonnet")
        self.assertEqual(result.agent, "claude-code")
        self.assertEqual(result.stop_reason, "end_turn")
        self.assertEqual(result.served_by, ["claude-code"])
        self.assertEqual(result.usage.input_tokens, 120)
        self.assertEqual(result.usage.output_tokens, 45)
        self.assertEqual(result.usage.cache_creation_tokens, 7)
        self.assertEqual(result.usage.cache_read_tokens, 300)
        self.assertAlmostEqual(result.cost_usd, 0.0123)
        self.assertEqual(result.duration_ms, 42)
        self.assertEqual(result.spec_gate, "on")
        self.assertEqual(result.exit_code, 0)
        self.assertIn("fake: running", result.stderr)
        with open(argv_file) as f:
            argv = json.load(f)
        self.assertEqual(
            argv,
            ["-t", "add a test", "--output-format", "json", "-w", self.ws, "-m", "sonnet", "-p", "read-only"],
        )

    def test_reported_failure_raises_task_failed_with_result_and_stderr(self):
        with self.assertRaises(TaskFailed) as caught:
            run_task("fail please", workspace=self.ws, incode_path=FAKE)
        e = caught.exception
        self.assertEqual(e.exit_code, 1)
        self.assertTrue(e.result.is_error)
        self.assertEqual(e.result.error, "the agent failed: fake failure")
        self.assertIsNone(e.result.cost_usd)
        self.assertIn("fake: running", e.stderr)
        self.assertIn("fake failure", str(e))

    def test_crash_without_result_raises_incode_error_with_stderr(self):
        with self.assertRaises(IncodeError) as caught:
            run_task("crash", workspace=self.ws, incode_path=FAKE)
        e = caught.exception
        self.assertNotIsInstance(e, TaskFailed)
        self.assertEqual(e.exit_code, 2)
        self.assertIn("could not start", e.stderr)
        self.assertIn("could not start", str(e))

    def test_bad_workspace_is_an_error(self):
        with self.assertRaises(IncodeError) as caught:
            run_task("x", workspace=os.path.join(self.ws, "missing"), incode_path=FAKE)
        self.assertIn("not a directory", caught.exception.stderr)

    def test_non_json_stdout_is_a_protocol_error(self):
        with self.assertRaises(ProtocolError):
            run_task("garbage", workspace=self.ws, incode_path=FAKE)

    def test_timeout_kills_and_raises(self):
        with self.assertRaises(IncodeTimeout):
            run_task("sleep", workspace=self.ws, incode_path=FAKE, timeout=1)

    def test_missing_binary(self):
        with self.assertRaises(IncodeError) as caught:
            run_task("x", incode_path=os.path.join(self.ws, "no-such-incode"))
        self.assertIn("could not start incode", str(caught.exception))

    def test_result_shape_of_the_real_binary_parses(self):
        # As printed by incode 0.1.0 for an empty instruction (cost_usd absent).
        line = (
            '{"type":"result","is_error":true,"session_id":"","agent":null,"model":null,'
            '"agent_session_id":null,"stop_reason":null,"served_by":[],"usage":{"input_tokens":0,'
            '"output_tokens":0,"cache_creation_tokens":0,"cache_read_tokens":0},"duration_ms":0,'
            '"spec_gate":"on","error":"nothing to do: the instruction is empty"}'
        )
        r = TaskResult.from_json(json.loads(line), exit_code=1)
        self.assertTrue(r.is_error)
        self.assertIsNone(r.cost_usd)
        self.assertIsNone(r.result)
        self.assertEqual(r.error, "nothing to do: the instruction is empty")
        self.assertEqual(r.raw["type"], "result")

    def test_argv(self):
        self.assertEqual(build_task_argv("hi"), ["incode", "-t", "hi", "--output-format", "json"])

    def test_async(self):
        result = asyncio.run(run_task_async("hello", workspace=self.ws, incode_path=FAKE))
        self.assertEqual(result.result, "done: hello")


if __name__ == "__main__":
    unittest.main()
