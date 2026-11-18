"""Unit tests for SynapsAgent.convert_trajectory's api-log attribution.

Fixtures are small synthetic daemon/api logs built here, never real benchmark logs.
Run from this directory with a Python that has Harbor 0.25:

    python -m unittest test_synaps_agent -v
"""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from harbor.models.agent.context import AgentContext  # noqa: E402

from synaps_agent import API_LOG, EVENTS_LOG, SynapsAgent, _now_iso  # noqa: E402

SID = "20990101-000000-aaaa"
RT = "session-1-1000-1"
CHILD_RT = "session-1-2000-3"


class Fixture:
    """A daemon event log plus api-log, written to a temp dir."""

    def __init__(self, session_id: str | None = SID):
        self.frames: list[dict] = [{"type": "welcome"}]
        if session_id is not None:
            self.frames.append({"type": "attached", "meta": {"id": session_id}})
        self.rows: list[dict] = []
        self.clock = 1_000_000
        self.n_parent = 0

    def _tick(self) -> int:
        self.clock += 1000
        return self.clock

    def _event(self, event: dict, session: str = SID) -> None:
        self.frames.append({"type": "event", "session_id": session,
                            "ts": f"2099-01-01T00:00:{len(self.frames):02d}Z", "event": event})

    def _stream(self, inner: dict, session: str = SID) -> None:
        self._event({"ev": "stream", "event": inner}, session)

    def turn(self, text: str) -> None:
        self._event({"ev": "turn_started", "trigger": "user", "user_text": text})

    def response(self, n: int, *, stop: str = "tool_use", tool: bool = True, row: bool = True,
                 attempt: int = 1, turn: int | None = None, session: str = SID, runtime: str = RT,
                 streamed_usage: tuple | None = None) -> dict:
        """One model response in `session` (usage derived from n) and its parent row."""
        usage = {"input": n, "output": 10 * n, "cache_read": 100 * n, "cache_write": 1000 * n}
        self._stream({"kind": "llm", "llm": "response_start"}, session)
        self._stream({"kind": "llm", "llm": "text", "text": f"response {n}"}, session)
        tid = f"toolu_{session[-4:]}_{n}_{attempt}"
        if tool:
            self._stream({"kind": "llm", "llm": "tool_use_start", "tool_name": "bash", "tool_id": tid}, session)
            self._stream({"kind": "llm", "llm": "tool_use", "tool_name": "bash", "tool_id": tid,
                          "input": {"command": f"echo {n}"}}, session)
        su = streamed_usage or (usage["input"], usage["output"], usage["cache_read"], usage["cache_write"])
        self._stream({"kind": "session", "session": "usage", "input_tokens": su[0], "output_tokens": su[1],
                      "cache_read_input_tokens": su[2], "cache_creation_input_tokens": su[3]}, session)
        if tool:
            self._stream({"kind": "llm", "llm": "tool_result", "tool_id": tid, "result": str(n)}, session)
        r = {"ts": self._tick(), "model": "claude-test", "attempt": attempt, "stop_reason": stop,
             "usage": usage, "cost_usd": n / 100, "session_id": session, "runtime_session_id": runtime,
             "turn_id": f"turn-1-{self.n_parent if turn is None else turn}",
             "trace_request_id": f"req-1-{self.n_parent if turn is None else turn}"}
        if turn is None:
            self.n_parent += 1
        if row:
            self.rows.append(r)
        return r

    def child_row(self, n: int, index: int, runtime: str = CHILD_RT) -> dict:
        """A subagent request: no session_id, own runtime session, same index scheme."""
        r = {"ts": self._tick(), "model": "claude-test", "attempt": 1, "stop_reason": "tool_use",
             "usage": {"input": n, "output": 10 * n, "cache_read": 100 * n, "cache_write": 1000 * n},
             "cost_usd": n / 100, "worker": True, "runtime_session_id": runtime,
             "turn_id": f"turn-1-{index}", "trace_request_id": f"req-1-{index}",
             "worker_agent": "reviewer", "worker_handle": "sa_1",
             "parent_session_id": SID, "parent_runtime_session_id": RT}
        self.rows.append(r)
        return r

    def refused(self, n: int, attempts: int = 3) -> list[dict]:
        """A request refused `attempts` times: one response_start and one row per attempt."""
        turn = self.n_parent
        rows = []
        for a in range(1, attempts + 1):
            rows.append(self.response(n, stop="refusal", tool=False, attempt=a, turn=turn))
            if a < attempts:
                self._stream({"kind": "session", "session": "notice", "retry": True,
                              "text": f"model refusal, retrying ({a}/{attempts - 1})"})
                self._stream({"kind": "llm", "llm": "response_reset"})
        self.n_parent += 1
        self._stream({"kind": "session", "session": "error", "message": "model refused the request"})
        return rows

    def done(self) -> None:
        self._stream({"kind": "session", "session": "done"})

    def write(self, d: Path, api_log: bool = True) -> Path:
        (d / EVENTS_LOG).write_text("".join(json.dumps(f) + "\n" for f in self.frames))
        if api_log:
            (d / API_LOG).write_text("".join(json.dumps(r) + "\n" for r in self.rows))
        return d


def _cost(rows: list[dict]) -> float:
    return sum(r["cost_usd"] for r in rows)


def _prompt(r: dict) -> int:
    u = r["usage"]
    return u["input"] + u["cache_read"] + u["cache_write"]


class ConvertTrajectoryTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self._tmp.name)
        self.agent = SynapsAgent(logs_dir=self.dir, model_name="anthropic/claude-test", version="0.0.0")

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def convert(self, fx: Fixture, api_log: bool = True):
        traj = self.agent.convert_trajectory(fx.write(self.dir, api_log))
        self.assertIsNotNone(traj)
        return traj

    @staticmethod
    def agent_steps(traj):
        return [s for s in traj.steps if s.source == "agent"]

    def assert_paired(self, traj, rows: list[dict]) -> None:
        steps = self.agent_steps(traj)
        self.assertEqual(len(steps), len(rows))
        for s, r in zip(steps, rows):
            self.assertIsNotNone(s.metrics, s.step_id)
            self.assertEqual(s.metrics.cost_usd, r["cost_usd"])
            self.assertEqual(s.metrics.completion_tokens, r["usage"]["output"])
            self.assertEqual(s.metrics.prompt_tokens, _prompt(r))
            self.assertEqual(s.metrics.extra["stop_reason"], r["stop_reason"])
            self.assertEqual(s.model_name, r["model"])
        self.assertEqual(traj.final_metrics.extra["attribution"]["status"], "paired")

    def assert_unpaired(self, traj, problem: str) -> None:
        att = traj.final_metrics.extra["attribution"]
        self.assertEqual(att["status"], "unpaired")
        self.assertIn(problem, att["problem"])
        for s in self.agent_steps(traj):
            self.assertIsNone(s.metrics)
            self.assertIsNone(s.model_name)
            self.assertTrue(s.timestamp.startswith("2099-01-01T00:00:"))  # the event's own time

    def assert_totals(self, traj, rows: list[dict]) -> None:
        fm = traj.final_metrics
        self.assertAlmostEqual(fm.total_cost_usd, _cost(rows))
        self.assertEqual(fm.total_prompt_tokens, sum(_prompt(r) for r in rows))
        self.assertEqual(fm.total_completion_tokens, sum(r["usage"]["output"] for r in rows))
        self.assertEqual(fm.total_cached_tokens, sum(r["usage"]["cache_read"] for r in rows))
        self.assertEqual(fm.extra["api_requests"], len(rows))

    # ── cases ───────────────────────────────────────────────────────────

    def test_single_session_unchanged(self):
        """No subagents: each response gets the row at its position, totals are
        the sums over steps, and the output is the old format plus extra."""
        fx = Fixture()
        fx.turn("task")
        parent = [fx.response(n) for n in (1, 2, 3)]
        parent.append(fx.response(4, stop="end_turn", tool=False))
        fx.done()
        traj = self.convert(fx)
        self.assertEqual([s.source for s in traj.steps], ["system", "user"] + ["agent"] * 4)
        self.assert_paired(traj, parent)
        steps = self.agent_steps(traj)
        self.assertEqual(steps[0].tool_calls[0].arguments, {"command": "echo 1"})
        self.assertEqual(steps[0].observation.results[0].content, "1")
        fm = traj.final_metrics
        self.assertEqual(fm.total_cost_usd, sum(s.metrics.cost_usd for s in steps))  # same float, same order
        self.assertEqual(fm.total_prompt_tokens, sum(s.metrics.prompt_tokens for s in steps))
        self.assertEqual(fm.total_steps, len(traj.steps))
        self.assertEqual(fm.extra["subagents"]["requests"], 0)
        self.assertNotIn("unattributed", fm.extra)

    def test_interleaved_parent_and_child_rows(self):
        fx = Fixture()
        fx.turn("task")
        parent = [fx.response(1)]
        child = [fx.child_row(50, 0)]
        parent.append(fx.response(2))
        child += [fx.child_row(51, 1), fx.child_row(52, 2)]
        parent.append(fx.response(3))
        child.append(fx.child_row(53, 3))
        parent.append(fx.response(4, stop="end_turn", tool=False))
        fx.done()
        traj = self.convert(fx)
        self.assert_paired(traj, parent)
        for s, r in zip(self.agent_steps(traj), parent):
            self.assertEqual(s.timestamp, _now_iso(r["ts"]))
        ex = traj.final_metrics.extra
        self.assertEqual(ex["parent"]["requests"], 4)
        self.assertEqual(ex["parent"]["session_id"], SID)
        self.assertEqual(ex["parent"]["runtime_session_ids"], [RT])
        self.assertEqual(ex["subagents"]["requests"], 4)
        self.assertEqual(ex["subagents"]["runtime_session_ids"], [CHILD_RT])
        self.assertAlmostEqual(ex["subagents"]["cost_usd"], _cost(child))

    def test_refusal_retry_attempts(self):
        """Each refused attempt is its own response and its own row."""
        fx = Fixture()
        fx.turn("task")
        parent = [fx.response(1)]
        parent += fx.refused(2)
        fx.turn("confirm")
        parent += fx.refused(3, attempts=2)
        traj = self.convert(fx)
        self.assertEqual([s.source for s in traj.steps], ["system", "user"] + ["agent"] * 4
                         + ["user"] + ["agent"] * 2)
        self.assert_paired(traj, parent)
        self.assertEqual([s.metrics.extra["stop_reason"] for s in self.agent_steps(traj)],
                         ["tool_use"] + ["refusal"] * 5)
        self.assert_totals(traj, parent)

    def test_equal_response_indices_in_two_sessions(self):
        """A child's req-1-0.. rows (logged first) and another session's frames and
        rows reuse the parent's indices; only the parent's own rows are paired."""
        other = "20990101-000000-bbbb"
        fx = Fixture()
        fx.turn("task")
        child = [fx.child_row(60, 0), fx.child_row(61, 1)]
        foreign = fx.response(70, session=other, runtime="session-1-5000-1", turn=0)  # index 0 again
        parent = [fx.response(1), fx.response(2, stop="end_turn", tool=False)]
        fx.done()
        traj = self.convert(fx)
        self.assertEqual(len(self.agent_steps(traj)), 2)  # the foreign response is not a step
        self.assert_paired(traj, parent)
        ex = traj.final_metrics.extra
        self.assertEqual(ex["attribution"]["other_session_frames"], 6)
        self.assertEqual(ex["unattributed"]["requests"], 1)
        self.assertEqual(ex["subagents"]["requests"], 2)
        self.assert_totals(traj, child + [foreign] + parent)

    def test_missing_attached_session_id(self):
        fx = Fixture(session_id=None)
        fx.turn("task")
        fx.response(1)
        fx.response(2)
        fx.child_row(50, 0)
        traj = self.convert(fx)
        self.assert_unpaired(traj, "no attached session id")
        self.assertIsNone(traj.session_id)
        ex = traj.final_metrics.extra
        self.assertEqual(ex["parent"]["requests"], 0)
        self.assertEqual(ex["unattributed"]["requests"], 2)
        self.assertEqual(ex["subagents"]["requests"], 1)
        self.assert_totals(traj, fx.rows)

    def test_rows_without_session_id(self):
        fx = Fixture()
        fx.turn("task")
        fx.response(1)
        fx.response(2)
        for r in fx.rows:
            del r["session_id"]
        traj = self.convert(fx)
        self.assert_unpaired(traj, "2 responses but 0 parent")
        self.assert_totals(traj, fx.rows)

    def test_count_mismatch(self):
        fx = Fixture()
        fx.turn("task")
        rows = [fx.response(1), fx.response(2)]
        fx.response(3, row=False)  # e.g. cancelled before its row was logged
        fx.child_row(50, 0)
        traj = self.convert(fx)
        self.assert_unpaired(traj, "3 responses but 2 parent")
        self.assertEqual(traj.final_metrics.extra["attribution"]["responses"], 3)
        self.assert_totals(traj, fx.rows)
        self.assertEqual(traj.final_metrics.extra["parent"]["requests"], len(rows))

    def test_no_api_log(self):
        fx = Fixture()
        fx.turn("task")
        fx.response(1)
        traj = self.convert(fx, api_log=False)
        self.assert_unpaired(traj, "no api-log")
        fm = traj.final_metrics
        self.assertEqual((fm.total_prompt_tokens, fm.total_cost_usd), (0, None))

    def test_streamed_usage_mismatch(self):
        """Same counts, but a response's own streamed usage contradicts its row."""
        fx = Fixture()
        fx.turn("task")
        fx.response(1)
        fx.response(2, streamed_usage=(9, 9, 9, 9))
        traj = self.convert(fx)
        self.assert_unpaired(traj, "streamed usage differs")

    def test_parent_runtime_session_conflict(self):
        fx = Fixture()
        fx.turn("task")
        fx.response(1)
        fx.response(2)["runtime_session_id"] = "session-1-9999-2"
        traj = self.convert(fx)
        self.assert_unpaired(traj, "span runtime sessions")

    def test_totals_count_each_row_once(self):
        fx = Fixture()
        fx.turn("task")
        parent = [fx.response(1)]
        child = [fx.child_row(50, 0), fx.child_row(51, 1)]
        parent.append(fx.response(2, stop="end_turn", tool=False))
        child.append(fx.child_row(52, 2, runtime="session-1-3000-5"))
        fx.write(self.dir)
        ctx = AgentContext()
        self.agent.populate_context_post_run(ctx)
        traj = json.loads((self.dir / "trajectory.json").read_text())
        fm = traj["final_metrics"]
        ex = fm["extra"]
        self.assertAlmostEqual(fm["total_cost_usd"], _cost(parent) + _cost(child))
        self.assertAlmostEqual(ex["parent"]["cost_usd"] + ex["subagents"]["cost_usd"], fm["total_cost_usd"])
        self.assertEqual(ex["parent"]["requests"] + ex["subagents"]["requests"], 5)
        self.assertEqual(ex["subagents"]["runtime_session_ids"], [CHILD_RT, "session-1-3000-5"])
        for k in ("prompt_tokens", "cached_tokens", "completion_tokens"):
            self.assertEqual(ex["parent"][k] + ex["subagents"][k], fm[f"total_{k}"])
        step_cost = sum(s["metrics"]["cost_usd"] for s in traj["steps"] if s.get("metrics"))
        self.assertAlmostEqual(step_cost, _cost(parent))  # steps: the attached session only
        self.assertEqual(ctx.cost_usd, fm["total_cost_usd"])
        self.assertEqual(ctx.n_input_tokens, fm["total_prompt_tokens"])
        self.assertEqual(ctx.n_cache_tokens, fm["total_cached_tokens"])
        self.assertEqual(ctx.n_output_tokens, fm["total_completion_tokens"])


if __name__ == "__main__":
    unittest.main()
