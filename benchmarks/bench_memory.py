#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
#
# Memory-footprint benchmark for terminal agent harnesses.
#
# Measures total PSS + SwapPss (Proportional Set Size, resident and swapped)
# across the full process tree of a tool running N concurrent sessions. SwapPss
# is included because with zram/swap the kernel swaps an idle TUI's pages out
# within seconds, and resident PSS alone then undercounts by pressure. PSS (from /proc/<pid>/smaps_rollup) splits
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
# Isolation: every synaps invocation runs against an isolated base dir
# (SYNAPS_BASE_DIR if set, else /tmp/synaps-bench-home); the harness refuses to
# run against ~/.synaps-cli, so it can never stop or attach to a live daemon.
# It also aborts if any pre-existing synaps daemon pid disappears mid-run.
#
# Requires: python3, pyte  (pip install pyte); Linux (/proc/*/smaps_rollup)
# Escaped processes: every launched process carries a BENCH_RUN_TAG env marker;
# tagged processes are summed even if they left the launched tree/pgroup
# (jcode's TUI starts `jcode serve` with setsid, which outlives its parent).
# jcode runs with an isolated JCODE_HOME/JCODE_RUNTIME_DIR per run.
#
# Usage:    python3 bench_memory.py --sessions 1 10 --reps 3 --tools synaps jcode
#           SYNAPS_BIN=/path/to/synaps python3 bench_memory.py --sessions 10

from __future__ import annotations

import argparse
import json
import os
import pty
import re
import select
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import time
import uuid
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


DEFAULT_BENCH_BASE = "/tmp/synaps-bench-home"


def bench_base_dir() -> Path:
    """Isolated Synaps base dir. Refuses anything that is, contains or lives in
    the real ~/.synaps-cli: the benchmark stops and restarts its daemon."""
    raw = os.environ.get("SYNAPS_BASE_DIR") or DEFAULT_BENCH_BASE
    base = Path(raw).expanduser().resolve()
    live = (Path.home() / ".synaps-cli").resolve()
    if base == live or live in base.parents or base in live.parents:
        raise SystemExit(f"refusing to benchmark in {base}: it overlaps the live base dir {live}. "
                         f"Unset SYNAPS_BASE_DIR or point it at a scratch dir (default {DEFAULT_BENCH_BASE}).")
    return base


def synaps_env(base: Path) -> dict[str, str]:
    """Environment for every synaps process: inherited SYNAPS_* (runtime dir,
    profile, session ids) is dropped so nothing can route to the live daemon."""
    env = {k: v for k, v in os.environ.items() if not k.startswith("SYNAPS_")}
    env["SYNAPS_BASE_DIR"] = str(base)
    return env


def synaps_daemon_pids() -> dict[int, str]:
    """Every running synaps daemon process: {pid: cmdline}."""
    out: dict[int, str] = {}
    for e in Path("/proc").iterdir():
        if not e.name.isdigit():
            continue
        try:
            argv = (e / "cmdline").read_bytes().split(b"\0")
        except OSError:
            continue
        if len(argv) > 1 and os.path.basename(argv[0]).startswith(b"synaps") and b"daemon" in argv[1:3]:
            out[int(e.name)] = b" ".join(argv).decode(errors="replace").strip()
    return out


def check_live_daemons(before: dict[int, str]) -> None:
    """Abort if a daemon that existed before the benchmark is gone."""
    now = synaps_daemon_pids()
    gone = {p: c for p, c in before.items() if p not in now}
    if gone:
        print(f"ABORT: pre-existing synaps daemon(s) disappeared during the run: {gone}", file=sys.stderr)
        raise SystemExit(2)


@dataclass
class ToolSpec:
    name: str
    argv: list[str]
    version_argv: list[str]
    env: dict[str, str] = field(default_factory=dict)
    synaps_daemon: bool = False
    jcode_home: bool = False  # isolated JCODE_HOME / JCODE_RUNTIME_DIR per run
    xdg_home: bool = False    # isolated XDG_{CONFIG,DATA,STATE,CACHE}_HOME per run


def build_specs() -> dict[str, ToolSpec]:
    synaps = os.environ.get("SYNAPS_BIN", "synaps")
    jcode = ["jcode", "--no-update", "--no-selfdev"]
    return {
        "synaps": ToolSpec("synaps", [synaps], [synaps, "--version"], synaps_daemon=True),
        # jcode's TUI spawns `jcode serve` in its own session (setsid); later
        # TUIs attach to it. Default config has features.memory = true.
        "jcode": ToolSpec("jcode", jcode, ["jcode", "version"],
                          {"JCODE_NO_TELEMETRY": "1"}, jcode_home=True),
        "jcode_memory_off": ToolSpec("jcode_memory_off", jcode, ["jcode", "version"],
                                     {"JCODE_NO_TELEMETRY": "1", "JCODE_MEMORY_ENABLED": "0"},
                                     jcode_home=True),
        "codex": ToolSpec("codex", ["codex"], ["codex", "--version"]),
        # opencode 2.x TUIs attach to a background `opencode serve --service`
        # found via $XDG_STATE_HOME/opencode/service.json (auto-started if
        # absent): isolated XDG dirs give it its own service, never the user's.
        "opencode": ToolSpec("opencode", ["opencode"], ["opencode", "--version"], xdg_home=True),
        "pi": ToolSpec("pi", ["pi"], ["pi", "--version"]),
        "claude_code": ToolSpec("claude_code", ["claude"], ["claude", "--version"]),
        "cursor": ToolSpec("cursor", ["cursor-agent"], ["cursor-agent", "--version"]),
    }


def tool_version(spec: ToolSpec) -> str:
    env = synaps_env(bench_base_dir()) if spec.synaps_daemon else os.environ.copy()
    try:
        p = subprocess.run(spec.version_argv, capture_output=True, text=True, timeout=30,
                           stdin=subprocess.DEVNULL, env=env)
    except (OSError, subprocess.TimeoutExpired) as e:
        return f"unavailable ({e.__class__.__name__})"
    lines = [l.strip() for l in (p.stdout + p.stderr).splitlines() if l.strip()]
    return " ".join(lines[0].split()) if lines else f"exit {p.returncode}"


def machine_info() -> dict:
    mem_kb = next(int(l.split()[1]) for l in Path("/proc/meminfo").read_text().splitlines()
                  if l.startswith("MemTotal:"))
    osr = dict(l.split("=", 1) for l in Path("/etc/os-release").read_text().splitlines() if "=" in l)
    model = next((l.split(":", 1)[1].strip() for l in Path("/proc/cpuinfo").read_text().splitlines()
                  if l.startswith("model name")), "?")
    return {"cpu": model, "cores": os.cpu_count(), "mem_gb": round(mem_kb / 1024 / 1024, 1),
            "os": osr.get("PRETTY_NAME", "?").strip('"'), "kernel": os.uname().release,
            "arch": os.uname().machine}


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
    ready: bool = False
    excerpt: str | None = None


def launch(argv: list[str], env: dict[str, str], timeout_s: float, settle_s: float) -> Launch:
    master, slave = pty.openpty()
    proc = subprocess.Popen(argv, stdin=slave, stdout=slave, stderr=slave,
                            env=env, preexec_fn=os.setsid)
    os.close(slave)
    os.set_blocking(master, False)
    start = time.perf_counter()
    buf = b""
    ready = probe_sent = False
    excerpt = None
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
                lines = [" ".join(l.split()) for l in plain.splitlines()]
                hit = next((l for l in lines if sum(c.isalnum() for c in l) >= 3), None)
                if not ready and hit:
                    ready, excerpt = True, hit[:100]
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
    return Launch(proc.pid, os.getpgid(proc.pid), master, ready, excerpt)


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


def pss_mb(pid: int) -> tuple[float, float] | None:
    """(Pss, SwapPss) in MB. SwapPss matters: with zram/swap, an idle TUI's
    anon pages are swapped out within seconds and vanish from Pss alone."""
    try:
        vals = {}
        for line in Path(f"/proc/{pid}/smaps_rollup").read_text().splitlines():
            k, _, rest = line.partition(":")
            if k in ("Pss", "SwapPss"):
                vals[k] = int(rest.split()[0]) / 1024.0
        return (vals["Pss"], vals.get("SwapPss", 0.0)) if "Pss" in vals else None
    except Exception:
        return None


def stale_bench_pids() -> set[int]:
    """Processes left by an earlier, crashed run of this harness (any tag)."""
    out: set[int] = set()
    for e in Path("/proc").iterdir():
        if not e.name.isdigit() or int(e.name) == os.getpid():
            continue
        try:
            if any(v.startswith(b"BENCH_RUN_TAG=") for v in (e / "environ").read_bytes().split(b"\0")):
                out.add(int(e.name))
        except OSError:
            continue
    return out


def tagged_pids(tag: str) -> set[int]:
    """Processes carrying this run's BENCH_RUN_TAG in their environment. This
    catches anything a tool detached from its tree/pgroup (setsid, double
    fork, reparenting to init), e.g. a background server spawned by the TUI."""
    needle = f"BENCH_RUN_TAG={tag}".encode()
    out: set[int] = set()
    for e in Path("/proc").iterdir():
        if not e.name.isdigit():
            continue
        try:
            if needle in (e / "environ").read_bytes().split(b"\0"):
                out.add(int(e.name))
        except OSError:
            continue
    return out


def cmdline(pid: int) -> str:
    try:
        return Path(f"/proc/{pid}/cmdline").read_bytes().replace(b"\0", b" ").decode(errors="replace").strip()
    except OSError:
        return "?"


def sum_tree_pss(roots: list[int], pgids: list[int], tag: str) -> dict:
    """Total PSS over: descendants of the roots, their process groups, and
    every process tagged with this run's environment marker."""
    tree = descendants(roots) | pgroup_pids(pgids)
    tagged = tagged_pids(tag)
    procs = []
    for pid in sorted(tree | tagged):
        v = pss_mb(pid)
        if v is not None:
            procs.append({"pid": pid, "pss_mb": round(v[0], 1), "swap_pss_mb": round(v[1], 1),
                          "mem_mb": round(v[0] + v[1], 1), "in_tree": pid in tree,
                          "cmd": cmdline(pid)[:120]})
    total = lambda key, pred=lambda p: True: round(sum(p[key] for p in procs if pred(p)), 1)
    return {"mem_mb": total("mem_mb"),            # Pss + SwapPss: the reported number
            "pss_mb": total("pss_mb"),            # resident only
            "swap_pss_mb": total("swap_pss_mb"),
            "process_count": len(procs),
            "outside_tree_mem_mb": total("mem_mb", lambda p: not p["in_tree"]),
            "processes": procs}


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


def reap_tagged(tag: str) -> list[int]:
    """Kill whatever this run left behind; returns survivors (should be [])."""
    for sig in (signal.SIGTERM, signal.SIGKILL):
        left = tagged_pids(tag)
        if not left:
            return []
        for pid in left:
            try:
                os.kill(pid, sig)
            except ProcessLookupError:
                pass
        time.sleep(0.5)
    return sorted(tagged_pids(tag))


def fresh_bench_base() -> Path:
    """Empty isolated base dir: no plugins, no sessions, no pins carried over
    from a previous run (pinned sessions are rehydrated on daemon start)."""
    base = bench_base_dir()
    shutil.rmtree(base, ignore_errors=True)
    base.mkdir(parents=True)
    return base


def stop_bench_daemon(bin_: str, base: Path) -> None:
    env = synaps_env(base)
    assert Path(env["SYNAPS_BASE_DIR"]).resolve() == bench_base_dir() == base
    subprocess.run([bin_, "daemon", "stop", "--force"], env=env,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)


def run_tool(spec: ToolSpec, sessions: int, timeout_s: float, settle_s: float) -> dict:
    tag = f"{os.getpid()}-{uuid.uuid4().hex[:12]}"
    env = os.environ.copy()
    env.update(spec.env)
    launches: list[Launch] = []
    cleanup: list[int] = []
    base = fresh_bench_base() if spec.synaps_daemon else None
    jroot = None
    try:
        if spec.synaps_daemon:
            bin_ = spec.argv[0]
            env = synaps_env(base)
            env["BENCH_RUN_TAG"] = tag
            sock = str(base / "run" / "daemon.sock")
            stop_bench_daemon(bin_, base)
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
            env["BENCH_RUN_TAG"] = tag
            if spec.jcode_home:
                # Isolated jcode state + sockets: never the user's ~/.jcode or
                # their running server. The TUI still spawns its own server as
                # it normally would; TEMP_SERVER/OWNER_PID make that server
                # exit with this harness as a backstop.
                jroot = Path(tempfile.mkdtemp(prefix="jcode-bench-"))
                (jroot / "home").mkdir()
                (jroot / "run").mkdir()
                env.update({"JCODE_HOME": str(jroot / "home"), "JCODE_RUNTIME_DIR": str(jroot / "run"),
                            "JCODE_TEMP_SERVER": "1", "JCODE_SERVER_OWNER_PID": str(os.getpid())})
            if spec.xdg_home:
                jroot = Path(tempfile.mkdtemp(prefix=f"{spec.name}-bench-"))
                for d in ("config", "data", "state", "cache"):
                    (jroot / d).mkdir()
                    env[f"XDG_{d.upper()}_HOME"] = str(jroot / d)
                # Pre-start the isolated service, as for the synaps daemon: on a
                # fresh state dir, TUIs launched before service.json exists each
                # start their own service. Its managed port is fixed (49374 by
                # default) and may be the user's own service: pick a free one.
                with socket.socket() as sk:
                    sk.bind(("127.0.0.1", 0))
                    port = sk.getsockname()[1]
                for cmd in (["service", "set", "port", str(port)], ["service", "start"]):
                    subprocess.run(["opencode", *cmd], env=env, timeout=60,
                                   stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                   stderr=subprocess.DEVNULL, check=False)
                if not wait_socket(str(jroot / "state" / "opencode" / "service.json"), timeout_s):
                    raise RuntimeError("opencode service did not become ready")
            for _ in range(sessions):
                lc = launch(spec.argv, env, timeout_s, settle_s)
                launches.append(lc)
                cleanup.append(lc.pgid)
            roots = [l.pid for l in launches]
        res = sum_tree_pss(roots, cleanup, tag)
        res.update({"tool": spec.name, "sessions": sessions,
                    "ready": sum(l.ready for l in launches),
                    "excerpt": next((l.excerpt for l in launches if l.excerpt), None)})
        return res
    finally:
        for l in launches:
            try:
                os.close(l.fd)
            except Exception:
                pass
        for pgid in reversed(cleanup):
            kill_pgroup(pgid)
        if base is not None:
            stop_bench_daemon(spec.argv[0], base)
        if spec.xdg_home and jroot is not None:
            subprocess.run(["opencode", "service", "stop"], env=env, timeout=60,
                           stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL, check=False)
        left = reap_tagged(tag)
        if left:
            print(f"WARNING: {spec.name}: processes survived cleanup: {left}", file=sys.stderr)
        if jroot is not None:
            shutil.rmtree(jroot, ignore_errors=True)
        if base is not None:
            shutil.rmtree(base, ignore_errors=True)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--sessions", type=int, nargs="+", required=True,
                    help="one or more session counts, e.g. --sessions 1 10")
    ap.add_argument("--tools", nargs="*", default=["synaps"])
    ap.add_argument("--reps", type=int, default=1, help="repetitions per (tool, sessions); median reported")
    ap.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT_S)
    ap.add_argument("--settle", type=float, default=DEFAULT_SETTLE_S)
    ap.add_argument("--json-out", default=None, help="write raw results (every repetition) here")
    args = ap.parse_args()
    specs = build_specs()
    # a kill/hangup must still run every run's cleanup (finally blocks)
    for sig in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, lambda *_: sys.exit(130))
    stale = stale_bench_pids()
    if stale:
        print(f"reaping {len(stale)} process(es) left by an earlier run: {sorted(stale)}", flush=True)
        for pid in stale:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        time.sleep(0.5)
    live = synaps_daemon_pids()
    print(f"pre-existing synaps daemons (must survive): {sorted(live) or 'none'}", flush=True)
    if "synaps" in args.tools:
        print(f"synaps base dir (isolated): {bench_base_dir()}", flush=True)
    payload = {"date_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
               "machine": machine_info(), "args": vars(args), "versions": {},
               "live_daemons_before": live, "summary": [], "runs": []}
    for name in args.tools:
        spec = specs.get(name)
        if not spec:
            print(f"unknown tool: {name}")
            continue
        payload["versions"][name] = tool_version(spec)
        print(f"=== {name} ({payload['versions'][name]}) ===", flush=True)
        for n in args.sessions:
            vals = []
            for rep in range(1, args.reps + 1):
                check_live_daemons(live)
                res = run_tool(spec, n, args.timeout, args.settle)
                check_live_daemons(live)
                res["rep"] = rep
                payload["runs"].append(res)
                vals.append(res["mem_mb"])
                print(f"  {n:>2} session(s) rep {rep}: {res['mem_mb']:7.1f} MB  "
                      f"(pss {res['pss_mb']} + swap {res['swap_pss_mb']}) "
                      f"procs={res['process_count']} outside-tree={res['outside_tree_mem_mb']} MB "
                      f"ready={res['ready']}/{n} :: {res['excerpt']}", flush=True)
            med = round(statistics.median(vals), 1)
            payload["summary"].append({"tool": name, "sessions": n, "median_mem_mb": med, "raw_mem_mb": vals})
            print(f"  {n:>2} session(s) median: {med} MB  raw={vals}", flush=True)
    payload["live_daemons_after"] = synaps_daemon_pids()
    if args.json_out:
        Path(args.json_out).write_text(json.dumps(payload, indent=2) + "\n")
        print(f"wrote {args.json_out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
