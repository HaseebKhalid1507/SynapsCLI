#!/usr/bin/env python3
"""Controllable, local-only Hello-first v2 sidecar for startup/cleanup tests.

Delays are milliseconds. --hello-gate waits for a file before Hello; marker
files let tests synchronize without relying on Python process startup timing.
By default Init produces a ready status, but this is not part of the handshake.
"""

import argparse
import json
import os
from pathlib import Path
import sys
import time


def emit(frame):
    print(json.dumps(frame), flush=True)


def mark(path, text="ready"):
    if path:
        Path(path).write_text(text, encoding="utf-8")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--hello-delay-ms", type=int, default=0)
    parser.add_argument("--hello-gate")
    parser.add_argument("--init-status-delay-ms", type=int, default=0)
    parser.add_argument("--no-init-status", action="store_true")
    parser.add_argument("--stall-stdin", action="store_true")
    parser.add_argument("--pid-file")
    parser.add_argument("--hello-sent-file")
    parser.add_argument("--init-received-file")
    args = parser.parse_args()
    if args.hello_delay_ms < 0 or args.init_status_delay_ms < 0:
        parser.error("delays must be nonnegative")

    mark(args.pid_file, str(os.getpid()))
    if args.hello_gate:
        while not Path(args.hello_gate).exists():
            time.sleep(0.01)
    time.sleep(args.hello_delay_ms / 1000)
    emit({
        "type": "hello",
        "protocol_version": 2,
        "extension": "slow-sidecar",
        "capabilities": ["insert-text", "status"],
    })
    mark(args.hello_sent_file)

    if args.stall_stdin:
        # Deliberately do not consume even one byte, including Init.
        while True:
            time.sleep(1)

    for line in sys.stdin:
        if not line.strip():
            continue
        msg = json.loads(line)
        typ = msg.get("type")
        if typ == "init":
            mark(args.init_received_file)
            if not args.no_init_status:
                time.sleep(args.init_status_delay_ms / 1000)
                emit({"type": "status", "state": "ready"})
        elif typ == "trigger":
            if msg.get("name") == "press":
                emit({"type": "status", "state": "active", "label": "Active"})
            elif msg.get("name") == "release":
                emit({"type": "status", "state": "processing"})
                emit({
                    "type": "insert_text",
                    "text": "hello from slow sidecar",
                    "mode": "final",
                })
        elif typ == "shutdown":
            break


if __name__ == "__main__":
    main()
