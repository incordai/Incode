import asyncio
import json
import os
import tempfile
import threading
import time
import unittest

from _support import FAKE

from incode_sdk import (
    AcpError,
    AgentCrashed,
    AvailableCommands,
    IncodeError,
    MessageChunk,
    ModeChanged,
    Plan,
    PromptResult,
    Session,
    ThoughtChunk,
    ToolCall,
    ToolCallUpdate,
    UnknownUpdate,
    Usage,
    UserMessageChunk,
)
from incode_sdk.aio import AsyncSession


def run(stream):
    """Every event of a prompt, and its result."""
    events = list(stream)
    assert isinstance(events[-1], PromptResult), events
    return events[:-1], events[-1]


class SessionTestBase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = os.path.realpath(self.tmp.name)
        self.ws = os.path.join(self.root, "ws")
        os.mkdir(self.ws)

    def tearDown(self):
        self.tmp.cleanup()

    def session(self, **kwargs):
        kwargs.setdefault("incode_path", FAKE)
        kwargs.setdefault("request_timeout", 10)
        return Session(self.ws, **kwargs)

    def started(self, **kwargs):
        """Started, and past the available_commands_update incode sends after answering session/new."""
        s = self.session(**kwargs)
        s.start()
        deadline = time.time() + 5
        while not any(isinstance(e, AvailableCommands) for e in s.idle_events):
            if time.time() > deadline:
                s.close()
                raise AssertionError("no available_commands_update")
            time.sleep(0.01)
        return s


class StartTest(SessionTestBase):
    def test_start_opens_a_session(self):
        argv_file = os.path.join(self.root, "argv.json")
        with self.session(model="opus", permission_mode="read-only", env={"FAKE_INCODE_ARGV": argv_file}) as s:
            self.assertEqual(s.session_id, "fake-session-1")
            self.assertEqual(s.protocol_version, 1)
            self.assertEqual(s.agent_info["name"], "incode")
            self.assertTrue(s.agent_capabilities["loadSession"])
            self.assertEqual(s.current_mode, "read-only")
            self.assertEqual([m["id"] for m in s.available_modes], ["read-only", "workspace-write", "danger-full-access"])
            self.assertEqual([o["id"] for o in s.config_options], ["agent", "role"])
            deadline = time.time() + 5
            while not s.idle_events and time.time() < deadline:
                time.sleep(0.02)
            self.assertIsInstance(s.idle_events[0], AvailableCommands)
            self.assertEqual(s.idle_events[0].commands[0]["name"], "run")
        with open(argv_file) as f:
            self.assertEqual(json.load(f), ["--acp", "-m", "opus", "-p", "read-only"])
        self.assertIsNotNone(s.returncode)

    def test_on_update_hears_idle_updates(self):
        heard = []
        got = threading.Event()
        with self.session(on_update=lambda e: (heard.append(e), got.set())):
            self.assertTrue(got.wait(5))
        self.assertIsInstance(heard[0], AvailableCommands)

    def test_a_refused_start_raises_with_stderr(self):
        with self.assertRaises(AgentCrashed) as caught:
            self.session(permission_mode="prompt").start()
        self.assertEqual(caught.exception.exit_code, 1)
        self.assertIn("--acp", caught.exception.stderr)

    def test_a_missing_workspace_is_refused_by_the_agent(self):
        s = Session(os.path.join(self.root, "missing"), incode_path=FAKE, request_timeout=10)
        with self.assertRaises(AcpError) as caught:
            s.start()
        self.assertEqual(caught.exception.code, -32602)
        self.assertEqual(caught.exception.method, "session/new")

    def test_missing_binary(self):
        with self.assertRaises(IncodeError):
            Session(self.ws, incode_path=os.path.join(self.root, "nope")).start()

    def test_load_replays_history(self):
        with self.session(load_session_id="kept-session") as s:
            self.assertEqual(s.session_id, "kept-session")
            kinds = [type(e) for e in s.idle_events]
            self.assertEqual(kinds[:2], [UserMessageChunk, MessageChunk])
            self.assertEqual(s.idle_events[0].text, "remember the parser")
        with self.assertRaises(AcpError):
            self.session(load_session_id="no-such").start()


class PromptTest(SessionTestBase):
    def test_prompt_streams_typed_events_then_the_result(self):
        with self.started() as s:
            stream = s.prompt("hello")
            events, result = run(stream)
            self.assertEqual(result.stop_reason, "end_turn")
            self.assertIs(stream.result, result)
            types = [type(e) for e in events]
            self.assertEqual(
                types,
                [ThoughtChunk, MessageChunk, ToolCall, ToolCallUpdate, Plan, Usage, UnknownUpdate, MessageChunk],
            )
            self.assertEqual(stream.text, "Hello (mode workspace-write, agent claude-code)\n")
            call, done, plan, usage, unknown = events[2], events[3], events[4], events[5], events[6]
            self.assertEqual((call.tool_call_id, call.kind, call.status, call.title), ("t1", "read", "in_progress", "Read a.txt"))
            self.assertEqual(call.raw_input, {"path": "a.txt"})
            self.assertEqual((done.tool_call_id, done.status, done.raw_output), ("t1", "completed", "contents of a"))
            self.assertEqual([(e.content, e.priority, e.status) for e in plan.entries][0], ("Read a.txt", "high", "completed"))
            self.assertEqual((usage.used, usage.size), (38833, 1000000))
            self.assertEqual(unknown.kind, "some_future_update")
            self.assertEqual(events[0].session_id, "fake-session-1")
            # A second prompt in the same session.
            self.assertEqual(s.prompt("hello again").wait().stop_reason, "end_turn")

    def test_content_blocks_are_accepted(self):
        with self.started() as s:
            stream = s.prompt([{"type": "text", "text": "hello"}])
            self.assertEqual(stream.wait().stop_reason, "end_turn")

    def test_one_prompt_at_a_time(self):
        with self.started() as s:
            stream = s.prompt("wait")
            with self.assertRaises(IncodeError):
                s.prompt("hello")
            s.cancel()
            self.assertEqual(stream.wait().stop_reason, "cancelled")

    def test_set_mode_and_config_option(self):
        with self.started() as s:
            s.set_mode("read-only")
            self.assertEqual(s.current_mode, "read-only")
            options = s.set_config_option("agent", "codex")
            agent = [o for o in options if o["id"] == "agent"][0]
            self.assertEqual(agent["currentValue"], "codex")
            stream = s.prompt("hello")
            stream.wait()
            self.assertIn("mode read-only, agent codex", stream.text)
            self.assertTrue(any(isinstance(e, ModeChanged) and e.mode_id == "read-only" for e in s.idle_events))
            with self.assertRaises(AcpError) as caught:
                s.set_mode("yolo")
            self.assertEqual(caught.exception.code, -32602)
            with self.assertRaises(AcpError):
                s.set_config_option("agent", "not-installed")

    def test_async_session(self):
        async def go():
            async with AsyncSession(self.ws, incode_path=FAKE) as s:
                seen = [e async for e in s.prompt("hello")]
                await s.set_mode("read-only")
                return seen

        seen = asyncio.run(go())
        self.assertIsInstance(seen[-1], PromptResult)
        self.assertEqual(seen[-1].stop_reason, "end_turn")


class PermissionTest(SessionTestBase):
    def test_default_rejects(self):
        with self.started() as s:
            stream = s.prompt("permission")
            events, result = run(stream)
            self.assertEqual(result.stop_reason, "end_turn")
            self.assertIn("permission: reject", stream.text)
            update = [e for e in events if isinstance(e, ToolCallUpdate)][0]
            self.assertEqual(update.status, "failed")

    def test_round_trip_allow(self):
        asked = []

        def decide(request):
            asked.append(request)
            return request.allow()

        with self.started(on_permission=decide) as s:
            stream = s.prompt("permission")
            stream.wait()
            self.assertIn("permission: allow", stream.text)
        request = asked[0]
        self.assertEqual(request.session_id, "fake-session-1")
        self.assertEqual(request.tool_call_id, "g1")
        self.assertIn("writing here needs the owner", request.title)
        self.assertEqual([o.kind for o in request.options], ["allow_once", "reject_once"])

    def test_bool_none_and_bad_answers(self):
        for decision, said in ((True, "permission: allow"), (False, "permission: reject"), (None, "permission: cancelled"), ("bogus", "permission: reject")):
            with self.started(on_permission=lambda r, d=decision: d) as s:
                stream = s.prompt("permission")
                stream.wait()
                self.assertIn(said, stream.text, decision)

    def test_a_raising_callback_rejects(self):
        def boom(request):
            raise RuntimeError("callback bug")

        with self.started(on_permission=boom) as s:
            stream = s.prompt("permission")
            stream.wait()
            self.assertIn("permission: reject", stream.text)
            self.assertIn("callback bug", str(s.callback_errors[0]))

    def test_cancel_answers_an_open_question_cancelled(self):
        asked = threading.Event()
        release = threading.Event()

        def slow(request):
            asked.set()
            release.wait(10)
            return request.allow()

        with self.started(on_permission=slow) as s:
            stream = s.prompt("permission-wait")
            self.assertTrue(asked.wait(5))
            s.cancel()
            result = stream.wait()
            release.set()
            self.assertEqual(result.stop_reason, "cancelled")
            self.assertIn("permission answer after cancel: cancelled", stream.text)


class FsTest(SessionTestBase):
    def fs_prompt(self, s, read_path, write_path):
        stream = s.prompt("fs %s %s" % (read_path, write_path))
        self.assertEqual(stream.wait().stop_reason, "end_turn")
        return stream.text

    def test_read_and_write_inside_the_workspace(self):
        with open(os.path.join(self.ws, "in.txt"), "w") as f:
            f.write("hello from the workspace")
        with self.started() as s:
            text = self.fs_prompt(s, os.path.join(self.ws, "in.txt"), os.path.join(self.ws, "sub", "out.txt"))
        self.assertIn("read: hello from the workspace", text)
        self.assertIn("write: ok", text)
        with open(os.path.join(self.ws, "sub", "out.txt")) as f:
            self.assertEqual(f.read(), "written by fake\n")

    def test_refused_outside_the_workspace(self):
        secret = os.path.join(self.root, "secret.txt")
        with open(secret, "w") as f:
            f.write("do not read")
        outside = os.path.join(self.root, "out.txt")
        os.symlink(self.root, os.path.join(self.ws, "escape"))
        cases = [
            (secret, outside),
            (os.path.join(self.ws, "..", "secret.txt"), os.path.join(self.ws, "..", "out.txt")),
            (os.path.join(self.ws, "escape", "secret.txt"), os.path.join(self.ws, "escape", "out.txt")),
            # A symlink followed by `..`: resolved as the OS does, not lexically.
            (self.ws + "/escape/ws/../secret.txt", self.ws + "/escape/ws/../out.txt"),
        ]
        with self.started() as s:
            for read_path, write_path in cases:
                text = self.fs_prompt(s, read_path, write_path)
                self.assertIn("read error: -32602 access denied", text)
                self.assertIn("write error: -32602 access denied", text)
                self.assertNotIn("do not read", text)
        self.assertFalse(os.path.exists(outside))

    def test_missing_file_is_resource_not_found(self):
        with self.started() as s:
            text = self.fs_prompt(s, os.path.join(self.ws, "nope.txt"), os.path.join(self.ws, "ok.txt"))
        self.assertIn("read error: -32002", text)

    def test_disabled_and_custom_handlers(self):
        writes = []
        with self.started(on_read_file=False, on_write_file=lambda req: writes.append(req)) as s:
            text = self.fs_prompt(s, "/anything", "/virtual/out.txt")
        self.assertIn("read error: -32601", text)
        self.assertIn("write: ok", text)
        self.assertEqual(writes[0].path, "/virtual/out.txt")
        self.assertEqual(writes[0].content, "written by fake\n")


class FailureTest(SessionTestBase):
    def test_cancel_ends_the_prompt_cancelled(self):
        with self.started() as s:
            stream = s.prompt("wait")
            first = next(stream)
            self.assertIsInstance(first, MessageChunk)
            self.assertEqual(first.text, "working")
            started = time.monotonic()
            stream.cancel()
            self.assertEqual(stream.wait().stop_reason, "cancelled")
            self.assertLess(time.monotonic() - started, 5)
            # The session is usable afterwards.
            self.assertEqual(s.prompt("hello").wait().stop_reason, "end_turn")

    def test_agent_crash_mid_prompt(self):
        with self.started() as s:
            stream = s.prompt("crash")
            first = next(stream)
            self.assertEqual(first.text, "about to crash")
            with self.assertRaises(AgentCrashed) as caught:
                next(stream)
            self.assertEqual(caught.exception.exit_code, 3)
            self.assertIn("crashed mid-prompt", caught.exception.stderr)
            with self.assertRaises(AgentCrashed):
                s.prompt("hello")
            with self.assertRaises(AgentCrashed):
                s.set_mode("read-only")

    def test_malformed_lines_are_skipped(self):
        heard = []
        with self.started(on_protocol_error=lambda line, e: heard.append(line)) as s:
            stream = s.prompt("malformed")
            self.assertEqual(stream.wait().stop_reason, "end_turn")
            self.assertEqual(stream.text, "still alive")
            self.assertEqual(s.prompt("hello").wait().stop_reason, "end_turn")
        self.assertEqual(len(heard), 4, heard)
        self.assertIn("this is not json", heard)

    def test_unknown_agent_request_is_method_not_found(self):
        with self.started() as s:
            stream = s.prompt("unknown-request")
            stream.wait()
        self.assertIn("terminal/create -> -32601", stream.text)

    def test_prompt_timeout(self):
        from incode_sdk import IncodeTimeout

        with self.started() as s:
            stream = s.prompt("wait", timeout=0.5)
            with self.assertRaises(IncodeTimeout):
                stream.wait()


if __name__ == "__main__":
    unittest.main()
