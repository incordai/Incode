#!/usr/bin/env python3
"""Drive incode from Python: one ACP session that streams a prompt.

    python3 sdk/examples/python_example.py /path/to/repo "Summarise this project"

INCODE_PATH picks the binary (default: `incode` on PATH). To try it without a
real agent: INCODE_PATH=sdk/testing/fake_incode.py python3 sdk/examples/python_example.py . hello
"""

import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "python"))

from incode_sdk import IncodeError, MessageChunk, Plan, PromptResult, Session, ToolCall, ToolCallUpdate  # noqa: E402


def ask(request):
    print("\n[permission] %s" % request.title)
    answer = input("allow? [y/N] ").strip().lower()
    return request.allow() if answer == "y" else request.reject()


def main():
    workspace = sys.argv[1] if len(sys.argv) > 1 else "."
    text = sys.argv[2] if len(sys.argv) > 2 else "Summarise this project in three sentences."
    incode = os.environ.get("INCODE_PATH", "incode")
    try:
        with Session(workspace, incode_path=incode, permission_mode="read-only", on_permission=ask) as session:
            print("session %s (mode %s)" % (session.session_id, session.current_mode))
            for event in session.prompt(text):
                if isinstance(event, MessageChunk):
                    print(event.text, end="", flush=True)
                elif isinstance(event, ToolCall):
                    print("\n[tool] %s" % event.title)
                elif isinstance(event, ToolCallUpdate) and event.status in ("completed", "failed"):
                    print("[tool %s] %s" % (event.tool_call_id, event.status))
                elif isinstance(event, Plan):
                    print("\n[plan] " + "; ".join("%s (%s)" % (e.content, e.status) for e in event.entries))
                elif isinstance(event, PromptResult):
                    print("\n-- stop reason: %s" % event.stop_reason)
    except KeyboardInterrupt:
        pass
    except IncodeError as e:
        print("incode failed: %s" % e, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
