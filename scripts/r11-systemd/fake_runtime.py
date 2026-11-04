#!/usr/bin/python3
"""A version witness only: no sockets, PLC VM, bus access, or subprocesses."""

import json
import os
from pathlib import Path
import signal
import time

VERSION = "__R11_VERSION__"
BEHAVIOR = "__R11_BEHAVIOR__"


def main():
    events = Path(os.environ["IA2_R11_EVENTS"])
    manager = os.environ["IA2_R11_MANAGER"]

    def record(event):
        row = {"event": event, "version": VERSION, "pid": os.getpid(),
               "manager": manager, "monotonic_ns": time.monotonic_ns(),
               "invocation_id": os.environ.get("INVOCATION_ID")}
        fd = os.open(str(events), os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
        try:
            os.write(fd, (json.dumps(row) + "\n").encode())
        finally:
            os.close(fd)

    stopped = False

    def stop(_signum, _frame):
        nonlocal stopped
        stopped = True

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    record("start")
    if BEHAVIOR == "crash-once":
        try:
            fd = os.open(str(events.parent / (manager + ".crashed-once")),
                         os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        except FileExistsError:
            pass
        else:
            os.close(fd)
            record("injected-crash")
            return 23

    while not stopped:
        time.sleep(0.05)
    record("stop")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
