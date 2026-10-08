#!/usr/bin/env python3
"""Prints what the helper really sends for a bash call, frame by frame.

The transcript a model sees is one step removed from the wire, so this talks to a
helper of its own and shows the answer text verbatim.  Any command may be passed;
the point is whether the exit_status line is present.

    python3 probe_frames.py EXE DIR 'exit 0' 'exit 3' 'kill -TERM $$'
"""

import json
import subprocess
import sys


def main():
    if len(sys.argv) < 4:
        raise SystemExit(__doc__)
    exe, work = sys.argv[1], sys.argv[2]
    commands = sys.argv[3:]

    helper = subprocess.Popen(
        [exe, "--chdir", work],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        bufsize=1,
    )

    def send(obj):
        body = json.dumps(obj)
        helper.stdin.write(f"{len(body)}\n{body}")
        helper.stdin.flush()

    def recv():
        size = int(helper.stdout.readline())
        return json.loads(helper.stdout.read(size))

    ready = recv()
    print(f"ready: {ready['text']}")

    missing = 0
    for i, command in enumerate(commands):
        send({"id": i + 1, "tool": "bash", "args": {"command": command}})
        answer = recv()
        text = answer.get("result") or answer.get("error") or ""
        lines = text.split("\n")
        report = [line for line in lines[:4] if line.startswith(("bash job=", "exit_status="))]
        if answer.get("ok") and "status=done" in text and len(report) < 2:
            missing += 1
        print(f"  {command!r:32} ok={answer.get('ok')} {report}")

    helper.stdin.close()
    helper.wait(timeout=10)
    print(f"done answers without an exit_status line: {missing}")


if __name__ == "__main__":
    main()
