#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
#
# Memory-footprint benchmark for terminal agent harnesses.
#
# Measures total PSS (Proportional Set Size) across the full process tree of a
# tool running N concurrent sessions. PSS (from /proc/<pid>/smaps_rollup) splits
# shared pages proportionally, so it is the correct metric for multi-process /
# daemon architectures where RSS would overcount shared memory.
#
# For synaps (a daemon architecture: `synaps --attach` shares one daemon across
# sessions), the daemon + its workers + every attached client are summed. Other
# tools launch N independent sessions; their trees are summed the same way.
#
# Methodology and portions of this harness are adapted from jcode's
# `bench_memory_cli.py` by Jeremy Huang (MIT). See benchmarks/NOTICE.
#
# Requires: python3, pyte  (pip install pyte); Linux (/proc/*/smaps_rollup)
# Usage:    python3 bench_memory.py --sessions 10 --tools synaps
#           SYNAPS_BIN=/path/to/synaps python3 bench_memory.py --sessions 10

from __future__ import annotations

import argparse
import json
import os
import pty
import re
import select
import signal
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path

PROBE = "zx91q"
DEFAULT_TIMEOUT_S = 20.0
DEFAULT_SETTLE_S = 3.0
ANSI_RE = re.compile(r"\x1B(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~]|\][^\x1b\x07]*(?:\x07|\x1b\\))")

CAP_REPLIES = [
    (b"\x1b[6n", b"\x1b[1;1R"), (b"\x1b[c", b"\x1b[?62;c"),
    (b"\x1b[>c", b"\x1b[>0;0;0c"), (b"\x1b[?u", b"\x1b[?0u"),
    (b"\x1b]10;?\x1b\\", b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\"),
    (b"\x1b]11;?\x1b\\", b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
    (b"\x1b[?2026$p", b"\x1b[?2026;1$y"), (b"\x1b[?2027$p", b"\x1b[?2027;1$y"),
    (b"\x1b[?2004$p", b"\x1b[?2004;1$y"),
]


@dataclass
class ToolSpec:
    name: str
    argv: list[str]
    env: dict[str, str] = field(default_factory=dict)
    synaps_daemon: bool = False


def build_specs() -> dict[str, ToolSpec]:
    synaps = os.environ.get("SYNAPS_BIN", "synaps")
    return {
        "synaps": ToolSpec("synaps", [synaps], synaps_daemon=True),
        "jcode": ToolSpec("jcode", ["jcode", "--no-update", "--no-selfdev"],
                          {"JCODE_NO_TELEMETRY": "1"}),
        "codex": ToolSpec("codex", ["codex"]),
        "opencode": ToolSpec("opencode", ["opencode"]),
        "pi": ToolSpec("pi", ["pi"]),
        "antigravity": ToolSpec("antigravity", ["antigravity"],
                                {"AGY_CLI_DISABLE_AUTO_UPDATE": "1"}),
        "claude_code": ToolSpec("claude_code", ["claude"]),
        "cursor": ToolSpec("cursor", ["cursor-agent"]),
    }


def answer_caps(fd: int, buf: bytes) -> bytes:
    changed = True
    while changed:
        changed = False
        for q, rep in CAP_REPLIES:
            if q in buf:
                os.write(fd, rep)
                buf = buf.replace(q, b"")
                changed = True
    return buf


@dataclass
class Launch:
    pid: int
    pgid: int
    fd: int


def launch(argv: list[str], env: dict[str, str], timeout_s: float, settle_s: float) -> Launch:
    master, slave = pty.openpty()
    proc = subprocess.Popen(argv, stdin=slave, stdout=slave, stderr=slave,
                            env=env, preexec_fn=os.setsid)
    os.close(slave)
    os.set_blocking(master, False)
    start = time.perf_counter()
    buf = b""
    ready = probe_sent = False
    while time.perf_counter() - start < timeout_s:
        r, _, _ = select.select([master], [], [], 0.05)
        if r:
            try:
                chunk = os.read(master, 65536)
            except BlockingIOError:
                chunk = b""
            if chunk:
                buf = answer_caps(master, buf + chunk)
                plain = ANSI_RE.sub("", buf.decode("utf-8", "replace"))
                if not ready and any(sum(c.isalnum() for c in " ".join(l.split())) >= 3
                                     for l in plain.splitlines()):
                    ready = True
                    if not probe_sent:
                        try:
                            os.write(master, PROBE.encode()); probe_sent = True
                        except OSError:
                            break
                if probe_sent and PROBE in plain:
                    break
        if proc.poll() is not None:
            break
    if ready:
        time.sleep(settle_s)
    return Launch(proc.pid, os.getpgid(proc.pid), master)


def proc_map() -> dict[int, tuple[int, int]]:
    out: dict[int, tuple[int, int]] = {}
    for e in Path("/proc").iterdir():
        if not e.name.isdigit():
            continue
        try:
            stat = (e / "stat").read_text()
            rest = stat[stat.rfind(")") + 2:].split()
            out[int(e.name)] = (int(rest[1]), int(rest[2]))  # ppid, pgid
        except Exception:
            continue
    return out


def descendants(roots: list[int]) -> set[int]:
    m = proc_map()
    kids: dict[int, list[int]] = {}
    for pid, (ppid, _) in m.items():
        kids.setdefault(ppid, []).append(pid)
    seen: set[int] = set()
    stack = list(roots)
    while stack:
        p = stack.pop()
        if p in seen:
            continue
        seen.add(p)
        stack.extend(kids.get(p, []))
    return seen


def pgroup_pids(pgids: list[int]) -> set[int]:
    want = set(pgids)
    return {pid for pid, (_, pgid) in proc_map().items() if pgid in want}


def pss_mb(pid: int) -> float | None:
    try:
        for line in Path(f"/proc/{pid}/smaps_rollup").read_text().splitlines():
            if line.startswith("Pss:"):
                return int(line.split()[1]) / 1024.0
    except Exception:
        return None
    return None


def sum_tree_pss(roots: list[int], pgids: list[int]) -> tuple[float, int]:
    pids = descendants(roots) | pgroup_pids(pgids)
    total = counted = 0.0
    for pid in sorted(pids):
        v = pss_mb(pid)
        if v is not None:
            total += v
            counted += 1
    return round(total, 1), int(counted)


def kill_pgroup(pgid: int) -> None:
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(pgid, sig)
            time.sleep(0.2)
        except ProcessLookupError:
            return


def wait_socket(path: str, timeout_s: float) -> bool:
    end = time.time() + timeout_s
    while time.time() < end:
        if Path(path).exists():
            return True
        time.sleep(0.05)
    return False


def run_tool(spec: ToolSpec, sessions: int, timeout_s: float, settle_s: float) -> dict:
    env = os.environ.copy()
    env.update(spec.env)
    launches: list[Launch] = []
    cleanup: list[int] = []
    try:
        if spec.synaps_daemon:
            bin_ = spec.argv[0]
            sock = str(Path.home() / ".synaps-cli" / "run" / "daemon.sock")
            subprocess.run([bin_, "daemon", "stop", "--force"],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
            time.sleep(0.5)
            server = subprocess.Popen([bin_, "daemon", "start"], env=env,
                                      stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                      stderr=subprocess.DEVNULL, preexec_fn=os.setsid)
            cleanup.append(os.getpgid(server.pid))
            if not wait_socket(sock, timeout_s):
                raise RuntimeError("synaps daemon did not become ready")
            # keep-warm attach clients (thin clients don't paint a detectable
            # banner; short launch timeout + explicit settle below)
            for _ in range(sessions):
                lc = launch([bin_, "--attach", "--new", "--keep-warm"], env, min(timeout_s, 4.0), settle_s)
                launches.append(lc)
                cleanup.append(lc.pgid)
            time.sleep(max(settle_s, 3.0))
            roots = [server.pid] + [l.pid for l in launches]
        else:
            for _ in range(sessions):
                lc = launch(spec.argv, env, timeout_s, settle_s)
                launches.append(lc)
                cleanup.append(lc.pgid)
            roots = [l.pid for l in launches]
        total, count = sum_tree_pss(roots, cleanup)
        return {"tool": spec.name, "sessions": sessions, "pss_mb": total, "process_count": count}
    finally:
        for l in launches:
            try:
                os.close(l.fd)
            except Exception:
                pass
        for pgid in reversed(cleanup):
            kill_pgroup(pgid)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--sessions", type=int, required=True)
    ap.add_argument("--tools", nargs="*", default=["synaps"])
    ap.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT_S)
    ap.add_argument("--settle", type=float, default=DEFAULT_SETTLE_S)
    args = ap.parse_args()
    specs = build_specs()
    results = []
    for name in args.tools:
        spec = specs.get(name)
        if not spec:
            print(f"unknown tool: {name}")
            continue
        print(f"=== {name} @ {args.sessions} session(s) ===", flush=True)
        res = run_tool(spec, args.sessions, args.timeout, args.settle)
        print(json.dumps(res, indent=2), flush=True)
        results.append(res)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
