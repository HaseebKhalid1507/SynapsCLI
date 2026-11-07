"""Completion modes of the Synaps Terminal-Bench harness.

Synthetic fixtures only: no docker, Harbor run, synaps binary or model call. The
driver runs against a fake `synaps` and `uds-relay` that script daemon events and
api-log rows.

    ~/harbor-synaps/tb41/.venv/bin/python -m unittest discover \
        -s benchmarks/terminal-bench/harness/tests -v
"""

from __future__ import annotations

import asyncio
import importlib.util
import json
import os
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HARNESS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(HARNESS))

import synaps_agent as sa  # noqa: E402
from harbor.models.agent.context import AgentContext  # noqa: E402

DRIVER = HARNESS / "synaps-run.sh"
PUBLISHED_REV = "dbf3d50b"  # harness of the published Terminal-Bench 4.1 run
PUBLISHED_CONFIRM = (
    "Automated message: no human is monitoring this session, and nobody will answer "
    "questions or approve anything. If any part of the task is not yet complete and "
    "verified, continue now and finish it, making any remaining decisions yourself. "
    "If it is complete and verified, reply with a one-line summary and stop."
)
PUBLISHED_COMMAND = "bash /installed-agent/sx-driver /installed-agent/prompts.jsonl /logs/agent"
INSTRUCTION = 'Write "hello" to /app/out.txt.\nThen stop.'
SID = "20990101-000000-abcd"
MODEL = "anthropic/claude-opus-5-5"


def published(path: str) -> str | None:
    r = subprocess.run(["git", "-C", str(HARNESS), "show", f"{PUBLISHED_REV}:./{path}"],
                       capture_output=True, text=True)
    return r.stdout if r.returncode == 0 else None


def jl(objs) -> str:
    return "".join(json.dumps(o, separators=(",", ":")) + "\n" for o in objs)


# ── host side: prompts.jsonl, completion.env, command, metadata ─────────────


class FakeEnv:
    def __init__(self) -> None:
        self.files: dict[str, bytes] = {}

    async def upload_file(self, src: str, target: str) -> None:
        self.files[target] = Path(src).read_bytes()


def run_agent(module, logs_dir: Path | None = None, **kwargs):
    agent = module.SynapsAgent(logs_dir=logs_dir or Path(tempfile.mkdtemp()),
                               model_name=MODEL, **kwargs)
    env, commands = FakeEnv(), []

    async def exec_as_agent(environment, command, **_):
        commands.append(command)

    agent.exec_as_agent = exec_as_agent
    asyncio.run(agent.run(INSTRUCTION, env, AgentContext()))
    return env.files, commands


class DefaultModeUnchanged(unittest.TestCase):
    def test_confirm_text_is_the_published_text(self):
        self.assertEqual(sa.CONFIRM, PUBLISHED_CONFIRM)

    def test_default_and_explicit_confirm_write_the_published_layout(self):
        expected = (json.dumps(INSTRUCTION) + "\n" + json.dumps(PUBLISHED_CONFIRM) + "\n").encode()
        for kwargs in ({}, {"completion": "confirm"}):
            files, commands = run_agent(sa, **kwargs)
            self.assertEqual(files, {"/installed-agent/prompts.jsonl": expected}, kwargs)
            self.assertEqual(commands, [PUBLISHED_COMMAND], kwargs)

    def test_matches_the_published_module(self):
        src = published("synaps_agent.py")
        if src is None:
            self.skipTest(f"git revision {PUBLISHED_REV} not available")
        path = Path(tempfile.mkdtemp()) / "synaps_agent_published.py"
        path.write_text(src)
        spec = importlib.util.spec_from_file_location("synaps_agent_published", path)
        old = sys.modules[spec.name] = importlib.util.module_from_spec(spec)  # pydantic resolves names there
        spec.loader.exec_module(old)
        self.assertEqual(sa.CONFIRM, old.CONFIRM)
        self.assertEqual(sa.SYSTEM_PROMPT, old.SYSTEM_PROMPT)
        self.assertEqual(run_agent(sa), run_agent(old))

    def test_system_prompt_is_mode_independent(self):
        # system.md is written by install() from SYSTEM_PROMPT; run() in closeout
        # mode uploads only prompts.jsonl and completion.env.
        files, _ = run_agent(sa, completion="closeout", refusal_note=True, agent_timeout_sec=600)
        self.assertEqual(sorted(files), ["/installed-agent/completion.env",
                                         "/installed-agent/prompts.jsonl"])
        self.assertEqual(sa.SYSTEM_PROMPT, sa.DEFAULT_SYSTEM_PROMPT + "\n\n" + sa.AUTONOMY + "\n")


class CloseoutHostSide(unittest.TestCase):
    def test_prompts_hold_only_the_instruction_and_command_adds_completion_env(self):
        files, commands = run_agent(sa, completion="closeout")
        self.assertEqual(files["/installed-agent/prompts.jsonl"],
                         (json.dumps(INSTRUCTION) + "\n").encode())
        self.assertEqual(commands, [PUBLISHED_COMMAND + " /installed-agent/completion.env"])

    def env_vars(self, **kwargs) -> dict[str, str]:
        text = sa.completion_env(sa.SynapsOptions(completion="closeout", **kwargs))
        out = subprocess.run(
            ["bash", "-c", '. /dev/stdin; printf "%s\\0" "$SX_TIMEOUT_S" "$SX_CLOSEOUT" '
             '"$SX_CONTINUE" "$SX_REFUSAL_NOTE"'],
            input=text, capture_output=True, text=True, check=True).stdout
        return dict(zip(("timeout", "closeout", "continue", "note"), out.split("\0")))

    def test_completion_env_round_trips_through_bash(self):
        v = self.env_vars()
        self.assertEqual(v["timeout"], "")
        self.assertEqual(v["note"], "")
        self.assertEqual(json.loads(v["closeout"]), sa.CLOSEOUT.format(time=""))
        self.assertEqual(json.loads(v["continue"]), sa.CONTINUE)
        v = self.env_vars(refusal_note=True, agent_timeout_sec=28800.0)
        self.assertEqual(v["timeout"], "28800")
        self.assertEqual(json.loads(v["note"]), sa.REFUSAL_NOTE)
        self.assertIn("Elapsed: @E@ of @T@ minutes (@R@ remaining).", json.loads(v["closeout"]))

    def test_messages_are_task_general(self):
        for text in (sa.CLOSEOUT, sa.CONTINUE, sa.REFUSAL_NOTE):
            for word in ("test", "grader", "hidden", "verifier", "benchmark"):
                self.assertNotIn(word, text.lower())
        self.assertNotIn("@", sa.CLOSEOUT.format(time=""))

    def test_flags_need_closeout_and_values_are_checked(self):
        for kwargs in ({"refusal_note": True}, {"agent_timeout_sec": 600},
                       {"completion": "loop"}, {"completion": "closeout", "agent_timeout_sec": 0}):
            with self.assertRaises(ValueError, msg=kwargs):
                sa.SynapsAgent(logs_dir=Path(tempfile.mkdtemp()), model_name=MODEL, **kwargs)


class Metadata(unittest.TestCase):
    EVENTS = [
        {"type": "attached", "client": 1, "meta": {"id": SID}},
        {"type": "event", "event": {"ev": "turn_started", "trigger": "user", "user_text": "x"}},
        {"type": "event", "event": {"ev": "stream", "event": {"kind": "llm", "llm": "response_start"}}},
        {"type": "event", "event": {"ev": "stream", "event": {"kind": "llm", "llm": "text", "text": "ok"}}},
        {"type": "event", "event": {"ev": "stream", "event": {"kind": "session", "session": "done"}}},
    ]

    def populate(self, decisions=None, **kwargs):
        logs = Path(tempfile.mkdtemp())
        (logs / sa.EVENTS_LOG).write_text(jl(self.EVENTS))
        if decisions is not None:
            (logs / sa.COMPLETION_LOG).write_text(jl(decisions) + "{truncated\n")
        agent = sa.SynapsAgent(logs_dir=logs, model_name=MODEL, **kwargs)
        ctx = AgentContext()
        agent.populate_context_post_run(ctx)
        traj = json.loads((logs / "trajectory.json").read_text())
        return ctx.metadata["completion"], traj["agent"]["extra"]

    def test_confirm_mode_is_recorded(self):
        meta, extra = self.populate()
        self.assertEqual(meta, {"mode": "confirm"})
        self.assertEqual(extra["completion"], {"mode": "confirm"})
        self.assertEqual(extra["thinking"], "xhigh")

    def test_closeout_flags_and_decisions_are_recorded(self):
        d = [{"after": 1, "end": "end_turn", "action": "closeout"}]
        meta, extra = self.populate(d, completion="closeout", refusal_note=True, agent_timeout_sec=600)
        want = {"mode": "closeout", "refusal_note": True, "agent_timeout_sec": 600.0, "decisions": d}
        self.assertEqual(meta, want)
        self.assertEqual(extra["completion"], want)


# ── driver decision functions (synaps-run.sh, sourced) ──────────────────────


def bash_fn(*args: str) -> str:
    return subprocess.run(["bash", "-c", 'source "$1"; shift; "$@"', "bash", str(DRIVER), *args],
                          capture_output=True, text=True, check=True).stdout.strip()


def end_event(kind: str, message: str = "x") -> dict:
    if kind == "aborted":
        return {"type": "event", "event": {"ev": "aborted"}}
    inner = {"kind": "session", "session": kind}
    if kind == "error":
        inner["message"] = message
    return {"type": "event", "event": {"ev": "stream", "event": inner}}


def turn(kind: str) -> list[dict]:
    return [{"type": "event", "event": {"ev": "turn_started", "trigger": "user", "user_text": "x"}},
            end_event(kind), {"type": "event", "event": {"ev": "idle"}}]


def row(stop_reason, session_id=SID) -> dict:
    return {"ts": 1, "model": "claude-opus-5-5", "attempt": 1, "stop_reason": stop_reason,
            "usage": {"input": 1, "output": 1}, "session_id": session_id,
            "runtime_session_id": "session-1-1-1", "turn_id": "turn-1-0"}


class TurnEnd(unittest.TestCase):
    def classify(self, events: list[dict] | None, rows: list[dict] | None, before: int = 0) -> str:
        d = Path(tempfile.mkdtemp())
        ev, api = d / "events.jsonl", d / "api-log.jsonl"
        if events is not None:
            ev.write_text(jl(events))
        if rows is not None:
            api.write_text(jl(rows))
        return bash_fn("turn_end", str(ev), str(api), SID, str(before))

    def test_states(self):
        cases = [
            (turn("done"), [row("tool_use"), row("end_turn")], 0, "end_turn done end_turn"),
            (turn("done"), [row("tool_use"), row("max_tokens")], 0, "max_tokens done max_tokens"),
            (turn("error"), [row("refusal")] * 3, 0, "refusal error refusal"),
            (turn("error"), [], 0, "error error -"),
            (turn("error"), [row("tool_use")], 0, "error error tool_use"),
            (turn("aborted"), [row("tool_use")], 0, "cancelled aborted tool_use"),
        ]
        for events, rows, before, want in cases:
            self.assertEqual(self.classify(events, rows, before), want, want)

    def test_unknown_or_missing_signals(self):
        cases = [
            (turn("done"), [], 0),                          # done, no api-log row
            (turn("done"), None, 0),                        # no api-log at all
            (turn("done"), [row("tool_use")], 0),           # unexpected stop_reason
            (turn("done"), [row(None)], 0),                 # stop_reason null
            (turn("done"), [row("refusal")], 0),            # refusal that didn't end in error
            (turn("done"), [row("end_turn")], 1),           # only a stale row from an earlier turn
            (None, [row("end_turn")], 0),                   # no event log
            ([{"type": "event", "event": {"ev": "idle"}}], [row("end_turn")], 0),  # no end event
        ]
        for events, rows, before in cases:
            self.assertEqual(self.classify(events, rows, before).split()[0], "unknown",
                             (events, rows, before))

    def test_only_rows_since_the_submit_and_of_this_session_count(self):
        rows = [row("max_tokens"), row("end_turn"), row("end_turn", session_id=None),
                row("end_turn", session_id="20990101-000000-ffff")]
        self.assertEqual(self.classify(turn("done"), rows[:1] + rows[2:]), "max_tokens done max_tokens")
        self.assertEqual(self.classify(turn("done"), rows, before=1), "end_turn done end_turn")

    def test_last_end_event_wins(self):
        self.assertEqual(self.classify(turn("done") + turn("error"), [row("end_turn")]).split()[0], "error")
        self.assertEqual(self.classify(turn("error") + turn("done"), [row("end_turn")]).split()[0], "end_turn")

    def test_escaped_event_text_inside_strings_is_ignored(self):
        fake = {"type": "event", "event": {"ev": "stream", "event": {
            "kind": "llm", "llm": "text", "text": '{"session":"done"}'}}}
        self.assertEqual(self.classify(turn("error") + [fake], [row("refusal")]).split()[0], "refusal")


class NextStep(unittest.TestCase):
    ENDS = ("end_turn", "max_tokens", "refusal", "error", "cancelled", "unknown")

    def step(self, end, cont=0, final=0, note=0, left=None) -> str:
        args = [end, str(cont), str(final), str(note)] + ([] if left is None else [str(left)])
        return bash_fn("next_step", *args)

    def test_each_end_state(self):
        self.assertEqual(self.step("end_turn"), "closeout")
        self.assertEqual(self.step("max_tokens"), "continue")
        self.assertEqual(self.step("refusal"), "none:refusal")
        self.assertEqual(self.step("refusal", note=1), "refusal_note")
        for end in ("error", "cancelled", "unknown"):
            self.assertEqual(self.step(end), f"none:{end}")
            self.assertEqual(self.step(end, note=1, left=10_000), f"none:{end}")

    def test_caps(self):
        self.assertEqual(self.step("max_tokens", cont=1), "continue")
        self.assertEqual(self.step("max_tokens", cont=2), "closeout")
        for end in self.ENDS:  # after the close-out or the refusal note: nothing
            for note in (0, 1):
                self.assertEqual(self.step(end, final=1, note=note), "none:final_sent", end)

    def test_refusal_never_gives_a_closeout(self):
        for cont in (0, 1, 2, 3):
            for final in (0, 1):
                for note in (0, 1):
                    for left in (None, 10, 10_000):
                        self.assertNotIn(self.step("refusal", cont, final, note, left),
                                         ("closeout", "continue"))

    def test_deadline(self):
        for end in ("end_turn", "max_tokens", "refusal"):
            for left in (59, 0, -5):
                self.assertEqual(self.step(end, note=1, left=left), "none:deadline", (end, left))
        self.assertEqual(self.step("end_turn", left=60), "closeout")


# ── driver end to end against a fake daemon ─────────────────────────────────

FAKE_SYNAPS = """\
import os, socket, sys
a, sock = sys.argv[1:], os.environ["FAKE_SOCK"]
if a[:1] == ["--version"]:
    print("synaps 0.11.1")
elif a[:2] == ["daemon", "--detach"]:
    socket.socket(socket.AF_UNIX).bind(sock)
    print(f"daemon started (pid 1, socket {sock})")
elif a[:2] == ["daemon", "status"]:
    print(f"socket: {sock}")
elif a[:2] == ["daemon", "stop"]:
    print("daemon stopped (pid 1)")
"""

# Replays one scripted turn per submit: api-log rows, then the end event, then idle.
FAKE_RELAY = """\
import json, os, sys
SID = %r
script = json.load(open(os.environ["FAKE_SCRIPT"]))
sent = open(os.environ["FAKE_SENT"], "a", buffering=1)
api = os.path.join(os.environ["HOME"], ".cache/synaps/api-log.jsonl")
os.makedirs(os.path.dirname(api), exist_ok=True)
def emit(o):
    sys.stdout.write(json.dumps(o, separators=(",", ":")) + "\\n"); sys.stdout.flush()
def event(e):
    emit({"type": "event", "session_id": SID, "event": e})
n = 0
for line in sys.stdin:
    sent.write(line)
    m = json.loads(line)
    if m["type"] == "hello":
        emit({"type": "welcome", "protocol_version": 3})
    elif m["type"] == "attach":
        emit({"type": "attached", "client": 1, "meta": {"id": SID, "name": None}})
    elif m["cmd"]["cmd"] == "submit":
        t = script[n] if n < len(script) else {"rows": ["end_turn"], "end": "done"}
        n += 1
        event({"ev": "turn_started", "trigger": "user", "user_text": m["cmd"]["text"]})
        with open(api, "a") as f:
            for r in t["rows"]:
                f.write(json.dumps({"stop_reason": r, "usage": {"output": 1}, "session_id": SID},
                                   separators=(",", ":")) + "\\n")
        if t["end"] == "aborted":
            event({"ev": "aborted"})
        else:
            event({"ev": "stream", "event": {"kind": "session", "session": t["end"]}})
        event({"ev": "idle"})
    elif m["cmd"].get("name") == "close":
        event({"ev": "ended", "reason": "client_quit"})
        emit({"type": "bye", "reason": None})
        break
""" % SID

DONE = {"rows": ["tool_use", "end_turn"], "end": "done"}
CUT = {"rows": ["tool_use", "max_tokens"], "end": "done"}
REFUSED = {"rows": ["tool_use", "refusal", "refusal", "refusal"], "end": "error"}
AUTH_ERROR = {"rows": [], "end": "error"}
CANCELLED = {"rows": ["tool_use"], "end": "aborted"}
NO_ROW = {"rows": [], "end": "done"}


class DriverRun:
    """Starts synaps-run.sh (or the published driver) in its own HOME with a fake daemon."""

    def __init__(self, script, driver_text=None, closeout=None):
        self.dir = d = Path(tempfile.mkdtemp(prefix="sxt-"))
        (d / "bin").mkdir()
        (d / "home").mkdir()
        (d / "app").mkdir()
        for name, src in (("synaps", FAKE_SYNAPS), ("uds-relay", FAKE_RELAY)):
            p = d / "bin" / name
            p.write_text(f"#!{sys.executable}\n{src}")
            p.chmod(0o755)
        driver = d / "sx-driver"
        driver.write_text(driver_text or DRIVER.read_text())
        (d / "script.json").write_text(json.dumps(script))
        prompts = (INSTRUCTION,) if closeout is not None else (INSTRUCTION, PUBLISHED_CONFIRM)
        (d / "prompts.jsonl").write_text("".join(json.dumps(p) + "\n" for p in prompts))
        args = ["bash", str(driver), str(d / "prompts.jsonl"), str(d / "logs")]
        if closeout is not None:
            (d / "completion.env").write_text(
                sa.completion_env(sa.SynapsOptions(completion="closeout", **closeout)))
            args.append(str(d / "completion.env"))
        env = {"PATH": f"{d / 'bin'}:{os.environ['PATH']}", "HOME": str(d / "home"),
               "FAKE_SOCK": str(d / "sock"), "FAKE_SCRIPT": str(d / "script.json"),
               "FAKE_SENT": str(d / "sent.jsonl")}
        self.proc = subprocess.Popen(args, cwd="/", env=env, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, text=True)
        self._done = False

    def wait(self):
        if not self._done:
            self.out, self.err = self.proc.communicate(timeout=180)
            for f in (self.proc.stdout, self.proc.stderr):
                f.close()
            self._done = True
        sent = (self.dir / "sent.jsonl").read_text()
        log = self.dir / "logs" / "completion.jsonl"
        decisions = [json.loads(x) for x in log.read_text().splitlines()] if log.exists() else None
        return sent, decisions

    def texts(self) -> list[str]:
        sent, _ = self.wait()
        return [m["cmd"]["text"] for m in map(json.loads, sent.splitlines())
                if m["type"] == "cmd" and m["cmd"]["cmd"] == "submit"]

    def actions(self) -> list[str]:
        _, decisions = self.wait()
        return [d["action"] if d["action"] != "none" else "none:" + d["why"] for d in decisions]


RUNS: dict[str, DriverRun] = {}


def setUpModule():
    old = published("synaps-run.sh")
    if old is not None:
        RUNS["confirm_published"] = DriverRun([DONE, DONE], driver_text=old)
    RUNS["confirm_new"] = DriverRun([DONE, DONE])
    RUNS["cut_thrice"] = DriverRun([CUT, CUT, CUT, DONE], closeout={})
    RUNS["done_timed"] = DriverRun([DONE, CUT], closeout={"agent_timeout_sec": 28800})
    RUNS["refused"] = DriverRun([REFUSED], closeout={})
    RUNS["refused_note"] = DriverRun([REFUSED, DONE], closeout={"refusal_note": True})
    RUNS["auth_error"] = DriverRun([AUTH_ERROR], closeout={"refusal_note": True})
    RUNS["cancelled"] = DriverRun([CANCELLED], closeout={})
    RUNS["no_row"] = DriverRun([NO_ROW], closeout={})
    RUNS["deadline"] = DriverRun([DONE], closeout={"agent_timeout_sec": 30})


def tearDownModule():
    for r in RUNS.values():
        if r.proc.poll() is None:
            r.proc.kill()


def normalized(sent: str) -> str:
    return re.sub(r'"instance":"[^"]*"', '"instance":"-"', sent)


class DriverConfirmMode(unittest.TestCase):
    def test_sends_are_byte_identical_to_the_published_driver(self):
        if "confirm_published" not in RUNS:
            self.skipTest(f"git revision {PUBLISHED_REV} not available")
        old, _ = RUNS["confirm_published"].wait()
        new, decisions = RUNS["confirm_new"].wait()
        self.assertEqual(normalized(new), normalized(old))
        self.assertIsNone(decisions)  # no completion.jsonl in confirm mode

    def test_sends(self):
        sent, _ = RUNS["confirm_new"].wait()
        kinds = [(m["type"], m.get("cmd", {}).get("cmd")) for m in map(json.loads, sent.splitlines())]
        self.assertEqual(kinds, [("hello", None), ("attach", None), ("cmd", "submit"),
                                 ("cmd", "submit"), ("cmd", "engine_command")])
        self.assertEqual(RUNS["confirm_new"].texts(), [INSTRUCTION, PUBLISHED_CONFIRM])
        self.assertEqual(RUNS["confirm_new"].proc.returncode, 0)


class DriverCloseoutMode(unittest.TestCase):
    def test_output_limit_continues_twice_then_closes_out_once(self):
        r = RUNS["cut_thrice"]
        self.assertEqual(r.texts(), [INSTRUCTION, sa.CONTINUE, sa.CONTINUE, sa.CLOSEOUT.format(time="")])
        self.assertEqual(r.actions(), ["continue", "continue", "closeout", "none:final_sent"])
        _, decisions = r.wait()
        self.assertEqual([d["end"] for d in decisions], ["max_tokens"] * 3 + ["end_turn"])
        self.assertEqual([d["after"] for d in decisions], [1, 2, 3, 4])

    def test_voluntary_end_gets_one_timed_closeout_and_nothing_after(self):
        r = RUNS["done_timed"]
        texts = r.texts()
        self.assertEqual(len(texts), 2)
        self.assertRegex(texts[1], r"^Automated close-out \(no human is present\)\. "
                                   r"Elapsed: 0 of 480 minutes \(47[89] remaining\)\. Before you stop")
        self.assertEqual(re.sub(r"Elapsed: .*? remaining\)\.", "Elapsed: @E@ of @T@ minutes (@R@ remaining).",
                                texts[1]), sa.CLOSEOUT.format(time=sa.CLOSEOUT_TIME))
        # The close-out turn was itself cut off: still no continuation, no second close-out.
        self.assertEqual(r.actions(), ["closeout", "none:final_sent"])

    def test_refusal_sends_nothing_by_default(self):
        self.assertEqual(RUNS["refused"].texts(), [INSTRUCTION])
        self.assertEqual(RUNS["refused"].actions(), ["none:refusal"])

    def test_refusal_note_is_sent_once_and_never_followed_by_a_closeout(self):
        r = RUNS["refused_note"]
        self.assertEqual(r.texts(), [INSTRUCTION, sa.REFUSAL_NOTE])
        self.assertEqual(r.actions(), ["refusal_note", "none:final_sent"])

    def test_error_cancel_deadline_and_unknown_send_nothing(self):
        for name, action in (("auth_error", "none:error"), ("cancelled", "none:cancelled"),
                             ("deadline", "none:deadline"), ("no_row", "none:unknown")):
            self.assertEqual(RUNS[name].texts(), [INSTRUCTION], name)
            self.assertEqual(RUNS[name].actions(), [action], name)

    def test_logs_keep_completion_env_and_driver_closes_the_session(self):
        r = RUNS["cut_thrice"]
        sent, _ = r.wait()
        self.assertEqual(json.loads(sent.splitlines()[-1])["cmd"]["name"], "close")
        self.assertTrue((r.dir / "logs" / "completion.env").is_file())
        self.assertEqual(r.proc.returncode, 0)


if __name__ == "__main__":
    unittest.main()
