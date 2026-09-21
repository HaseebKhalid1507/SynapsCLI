#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
#
# Startup-latency benchmark for terminal agent harnesses.
#
# Measures two user-visible metrics across N interactive PTY launches:
#   1. time to first rendered frame  (first meaningful line on screen)
#   2. time to input-ready           (typed probe text echoes back to screen)
#
# The harness drives a real pseudo-terminal, answers the terminal-capability
# queries a modern TUI emits at startup (without which you measure the query
# timeout, not the tool), renders output through a terminal screen model
# (pyte), and detects when meaningful content and input echo appear.
#
# Methodology and portions of this harness are adapted from jcode's
# `bench_startup_visible_ready.py` by Jeremy Huang (MIT). See benchmarks/NOTICE.
#
# Requires: python3, pyte  (pip install pyte)
# Usage:    python3 bench_latency.py --runs 10 --tools synaps
#           SYNAPS_BIN=/path/to/synaps python3 bench_latency.py --runs 10

from __future__ import annotations

import argparse
import os
import pty
import select
import signal
import statistics
import struct
import subprocess
import sys
import termios
import time
from dataclasses import dataclass, field

try:
    import fcntl
except ImportError as exc:  # pragma: no cover
    raise SystemExit(f"fcntl unavailable: {exc}")

try:
    import pyte
except ImportError as exc:  # pragma: no cover
    raise SystemExit("pyte is required: pip install pyte\n" f"import error: {exc}")

PROBE = "zx91q"
DEFAULT_RUNS = 10
DEFAULT_TIMEOUT_S = 10.0

# Query -> canned reply. A TUI that probes the terminal at boot blocks until it
# gets answers; we answer them so the measurement reflects the tool, not the wait.
CAP_REPLIES = [
    (b"\x1b[6n", b"\x1b[1;1R"),
    (b"\x1b[c", b"\x1b[?62;c"),
    (b"\x1b[>c", b"\x1b[>0;0;0c"),
    (b"\x1b[?u", b"\x1b[?0u"),
    (b"\x1b]10;?\x1b\\", b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\"),
    (b"\x1b]11;?\x1b\\", b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
    (b"\x1b]10;?\x07", b"\x1b]10;rgb:ffff/ffff/ffff\x07"),
    (b"\x1b]11;?\x07", b"\x1b]11;rgb:0000/0000/0000\x07"),
    (b"\x1b[14t", b"\x1b[4;600;800t"),
    (b"\x1b[16t", b"\x1b[6;16;8t"),
    (b"\x1b[18t", b"\x1b[8;24;80t"),
    (b"\x1b[?1016$p", b"\x1b[?1016;1$y"),
    (b"\x1b[?2026$p", b"\x1b[?2026;1$y"),
    (b"\x1b[?2027$p", b"\x1b[?2027;1$y"),
    (b"\x1b[?2004$p", b"\x1b[?2004;1$y"),
]


@dataclass
class ToolSpec:
    name: str
    argv: list[str]
    env: dict[str, str] = field(default_factory=dict)


def build_specs(args: argparse.Namespace) -> dict[str, ToolSpec]:
    synaps = os.environ.get("SYNAPS_BIN", "synaps")
    return {
        "synaps": ToolSpec("synaps", [synaps]),
        "jcode": ToolSpec("jcode", ["jcode", "--no-update", "--no-selfdev"],
                          {"JCODE_NO_TELEMETRY": "1"}),
        "codex": ToolSpec("codex", ["codex"]),
        "opencode": ToolSpec("opencode", ["opencode"]),
        "pi": ToolSpec("pi", ["pi"]),
    }


def configure_pty(fd: int, rows: int = 24, cols: int = 80) -> None:
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    attrs = termios.tcgetattr(fd)
    attrs[3] &= ~(termios.ECHO | termios.ICANON)
    attrs[0] &= ~(termios.ICRNL | termios.IXON)
    termios.tcsetattr(fd, termios.TCSANOW, attrs)


def answer_caps(fd: int, buf: bytes) -> bytes:
    changed = True
    while changed:
        changed = False
        for query, reply in CAP_REPLIES:
            if query in buf:
                os.write(fd, reply)
                buf = buf.replace(query, b"")
                changed = True
    return buf


def first_meaningful_line(screen: "pyte.Screen") -> str | None:
    for line in screen.display:
        norm = " ".join(line.split())
        if not norm or PROBE in norm:
            continue
        if sum(c.isalnum() for c in norm) >= 3 and len(norm) >= 4:
            return norm[:120]
    return None


def run_once(spec: ToolSpec, timeout_s: float) -> dict[str, object]:
    master, slave = pty.openpty()
    configure_pty(slave)
    env = os.environ.copy()
    env.update({"TERM": "xterm-256color", "COLORTERM": "truecolor"})
    env.update(spec.env)
    proc = subprocess.Popen(spec.argv, stdin=slave, stdout=slave, stderr=slave,
                            env=env, preexec_fn=os.setsid)
    os.close(slave)
    os.set_blocking(master, False)

    screen = pyte.Screen(80, 24)
    stream = pyte.Stream(screen)
    start = time.perf_counter()
    buf = b""
    first_visible_ms = input_ready_ms = None
    excerpt = None
    probe_sent = False
    try:
        while time.perf_counter() - start < timeout_s:
            r, _, _ = select.select([master], [], [], 0.05)
            if r:
                try:
                    chunk = os.read(master, 65536)
                except BlockingIOError:
                    chunk = b""
                if chunk:
                    buf = answer_caps(master, buf + chunk)
                    stream.feed(chunk.decode("utf-8", "replace"))
            if first_visible_ms is None:
                excerpt = first_meaningful_line(screen)
                if excerpt:
                    first_visible_ms = (time.perf_counter() - start) * 1000
                    os.write(master, PROBE.encode())
                    probe_sent = True
            elif probe_sent and input_ready_ms is None:
                if PROBE in "\n".join(screen.display):
                    input_ready_ms = (time.perf_counter() - start) * 1000
                    break
        return {"first_visible_ms": first_visible_ms,
                "input_ready_ms": input_ready_ms, "excerpt": excerpt}
    finally:
        for sig in (signal.SIGTERM, signal.SIGKILL):
            try:
                os.killpg(proc.pid, sig)
                time.sleep(0.1)
            except ProcessLookupError:
                break
        os.close(master)


def stats(values: list[float]) -> dict[str, float] | None:
    vals = [v for v in values if v is not None]
    if not vals:
        return None
    return {"median": statistics.median(vals), "min": min(vals), "max": max(vals), "n": len(vals)}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--runs", type=int, default=DEFAULT_RUNS)
    ap.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT_S)
    ap.add_argument("--tools", nargs="*", default=["synaps"])
    args = ap.parse_args()
    specs = build_specs(args)

    for name in args.tools:
        spec = specs.get(name)
        if not spec:
            print(f"unknown tool: {name}", file=sys.stderr)
            continue
        print(f"=== {name} ===", flush=True)
        vis, ready = [], []
        for i in range(1, args.runs + 1):
            res = run_once(spec, args.timeout)
            vis.append(res["first_visible_ms"])
            ready.append(res["input_ready_ms"])
            print(f"  run {i}/{args.runs}: visible={res['first_visible_ms']} "
                  f"ready={res['input_ready_ms']} :: {res['excerpt']}", flush=True)
        v, r = stats(vis), stats(ready)
        if v:
            print(f"  first-visible ms: median={v['median']:.1f} "
                  f"range={v['min']:.1f}-{v['max']:.1f} (n={v['n']})")
        if r:
            print(f"  input-ready  ms: median={r['median']:.1f} "
                  f"range={r['min']:.1f}-{r['max']:.1f} (n={r['n']})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
