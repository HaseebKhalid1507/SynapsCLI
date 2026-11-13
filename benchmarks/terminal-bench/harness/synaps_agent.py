"""Harbor (0.25) installed-agent integration for SynapsCLI, Terminal-Bench 4.1.

Run from this directory:

    PYTHONPATH=. .venv/bin/harbor run -d terminal-bench/terminal-bench@4.1.0 \
        --agent-import-path synaps_agent:SynapsAgent \
        -m anthropic/claude-opus-5-5 --ak reasoning_effort=xhigh -k 5 ...

Per trial:
  * uploads a pinned static (musl) synaps binary into the task container,
  * writes a minimal config (model, thinking, telemetry) and system.md
    (synaps's default prompt, verbatim, plus AUTONOMY),
  * points synaps at the host's auth broker: containers fetch short-lived
    access tokens and never hold an OAuth credential,
  * runs the task through the synaps daemon, the way Synaps is used day to
    day (synaps-run.sh starts `synaps daemon`, attaches over its socket via
    the static uds-relay and creates a session with the TUI's settings): the
    instruction, then one fixed CONFIRM message (same text for every task),
    then /close (saved) and daemon stop,
  * converts the daemon event log to an ATIF trajectory (trajectory.json),
    which the leaderboard requires for every passing trial.

Nothing here is task-specific: the prompt, the confirmation message and the
config are identical for all tasks, and no timeout/resource setting is touched.

Environment (host side):
  SYNAPS_BINARY             synaps binary to install (default: ./bin/.../synaps)
  SYNAPS_UDS_RELAY          static uds-relay binary (default: ./bin/uds-relay,
                            built from ./uds-relay for x86_64-unknown-linux-musl)
  SYNAPS_BROKER_URL         auth broker as seen from containers
                            (default: http://172.17.0.1:8181, the docker0 bridge)
  SYNAPS_BROKER_TOKEN_FILE  machine token file (default: ./broker/machine-token)
"""

from __future__ import annotations

import json
import os
import re
import shlex
import tempfile
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Literal, override

from pydantic import Field

from harbor.agents.capabilities import AgentCapabilities
from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
from harbor.agents.options import InstalledAgentOptions
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext
from harbor.models.trajectories import (
    Agent,
    FinalMetrics,
    Metrics,
    Observation,
    ObservationResult,
    Step,
    ToolCall,
    Trajectory,
)
from harbor.utils.trajectory_utils import format_trajectory_json

HERE = Path(__file__).resolve().parent
DEFAULT_BINARY = HERE / "bin" / "synaps-x86_64-unknown-linux-musl" / "synaps"
DRIVER = HERE / "synaps-run.sh"
DEFAULT_RELAY = HERE / "bin" / "uds-relay"
DEFAULT_MCP_BRIDGE = HERE / "bin" / "mcp-sse-bridge"

INSTALL_DIR = "/installed-agent"  # created as root by BaseInstalledAgent.setup
LOG_DIR = "/logs/agent"
EVENTS_LOG = "daemon-events.jsonl"
API_LOG = "api-log.jsonl"

# synaps 0.11.1's built-in default system prompt (non-Windows), verbatim.
# A system.md replaces it, so the run keeps it and appends AUTONOMY.
DEFAULT_SYSTEM_PROMPT = (
    "You are a helpful AI agent running in a terminal. You have access to bash, "
    "read, and write tools. Be concise and direct. Use tools when the user asks you "
    "to interact with the filesystem or run commands. Change existing files with "
    "targeted edits rather than rewriting them whole. A file you just read or wrote "
    "is current in your context; re-read it only if it may have changed. Keep files, "
    "docs and comments as lean as the task allows. Keep command output small (quiet "
    "flags, `| tail -n`, `grep`, `head`) and read large outputs selectively."
)
AUTONOMY = """\
# Autonomous run

This is an autonomous, non-interactive run in an isolated task environment. No human \
is present: nobody will read your messages, answer questions, approve actions or \
review your work. There is no separate submit or approval step. When you end your \
turn, the files and system state you leave behind are checked automatically, exactly \
as they are.

- Never end your turn with a question, a request for confirmation or permission, or \
an offer to do more ("Shall I...", "Let me know if...", "Would you like..."). Nobody \
will answer. Decide and act.
- Where the instructions leave a choice open, choose the most reasonable option and \
carry on.
- Before you finish, check your result yourself against every requirement in the \
task: run it, test it, and fix whatever fails. Do not stop at a plan or a partial \
result.
- End your turn only when the task is fully complete, with a short summary of what \
you did."""
SYSTEM_PROMPT = DEFAULT_SYSTEM_PROMPT + "\n\n" + AUTONOMY + "\n"

# Sent once after the first turn ends, identical for every task (like Terminus 2's
# completion confirmation). Recovers turns that stopped early: a question to a human
# who isn't there, a response cut off at the output-token limit.
CONFIRM = (
    "Automated message: no human is monitoring this session, and nobody will answer "
    "questions or approve anything. If any part of the task is not yet complete and "
    "verified, continue now and finish it, making any remaining decisions yourself. "
    "If it is complete and verified, reply with a one-line summary and stop."
)

Effort = Literal["low", "medium", "high", "xhigh", "max"]


class SynapsOptions(InstalledAgentOptions):
    reasoning_effort: Effort | None = Field(
        default=None, description="synaps `thinking` level (default: xhigh)."
    )


def _now_iso(ms: int | None = None) -> str:
    t = datetime.fromtimestamp(ms / 1000, timezone.utc) if ms else datetime.now(timezone.utc)
    return t.isoformat().replace("+00:00", "Z")


# ── api-log attribution ─────────────────────────────────────────────────
# api-log.jsonl holds one row per model request (refused attempts included) for
# the attached session *and* for every subagent it spawned, interleaved in time.
# Parent rows carry the attached session's `session_id`; subagent rows carry none,
# but have their own `runtime_session_id` and name their parent
# (`worker`, `parent_session_id`, `parent_runtime_session_id`).

def _read_api_log(path: Path) -> list[dict[str, Any]] | None:
    """Rows that carry usage, in file order; None if there is no log."""
    if not path.exists():
        return None
    rows = []
    for line in path.read_text(errors="replace").splitlines():
        try:
            row = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(row, dict) and row.get("usage"):
            rows.append(row)
    return rows


def _split_rows(session_id: str | None, rows: list[dict[str, Any]]):
    """(parent, subagent, unattributed) rows, each in file order."""
    parent, sub, other = [], [], []
    for r in rows:
        if session_id and r.get("session_id") == session_id:
            parent.append(r)
        elif r.get("worker") or r.get("parent_session_id") or r.get("parent_runtime_session_id"):
            sub.append(r)
        else:
            other.append(r)
    return parent, sub, other


def _prompt_tokens(u: dict[str, Any]) -> int:
    return (u.get("input") or 0) + (u.get("cache_read") or 0) + (u.get("cache_write") or 0)


def _row_usage(row: dict[str, Any]) -> tuple[int, ...]:
    u = row["usage"]
    return tuple(u.get(k) or 0 for k in ("input", "output", "cache_read", "cache_write"))


def _event_usage(ev: dict[str, Any]) -> tuple[int, ...]:
    return tuple(ev.get(k) or 0 for k in ("input_tokens", "output_tokens",
                                          "cache_read_input_tokens", "cache_creation_input_tokens"))


def _runtime_ids(rows: list[dict[str, Any]]) -> list[str | None]:
    return list(dict.fromkeys(r.get("runtime_session_id") for r in rows))


def _pair_responses(
    session_id: str | None,
    rows: list[dict[str, Any]] | None,
    parent: list[dict[str, Any]],
    usages: list[tuple[int, ...] | None],
) -> tuple[list[dict[str, Any] | None], dict[str, Any]]:
    """Pair the attached session's responses (`usages`: each one's streamed usage,
    if seen) with the parent rows, in order: the daemon streams one
    response_start per request. All or nothing: with no identity, an
    inconsistent one, a count mismatch or a usage mismatch, no response is
    paired and the reason is returned, instead of guessing by position."""
    n = len(usages)
    runtimes = _runtime_ids(parent)
    info: dict[str, Any] = {"responses": n, "parent_rows": len(parent)}
    problem = None
    if rows is None:
        problem = f"no {API_LOG}"
    elif not session_id:
        problem = "no attached session id"
    elif len(runtimes) > 1:
        problem = f"parent rows span runtime sessions {runtimes}"
    elif runtimes and runtimes[0] and any(
        r.get("runtime_session_id") == runtimes[0] and r.get("session_id") != session_id for r in rows
    ):
        problem = f"runtime session {runtimes[0]} also on rows without session id {session_id}"
    elif len(parent) != n:
        problem = f"{n} responses but {len(parent)} parent {API_LOG} rows"
    else:
        info["usage_checked"] = sum(u is not None for u in usages)
        bad = [i for i, (u, r) in enumerate(zip(usages, parent)) if u is not None and u != _row_usage(r)]
        if bad:
            problem = f"streamed usage differs from the paired row for responses {bad[:5]}"
    if problem:
        info.update(status="unpaired", problem=problem)
        return [None] * n, info
    info["status"] = "paired"
    return list(parent), info


def _tally(rows: list[dict[str, Any]]) -> dict[str, Any]:
    cost = sum(r.get("cost_usd") or 0 for r in rows)
    return {
        "requests": len(rows),
        "prompt_tokens": sum(_prompt_tokens(r["usage"]) for r in rows),
        "cached_tokens": sum(r["usage"].get("cache_read") or 0 for r in rows),
        "completion_tokens": sum(r["usage"].get("output") or 0 for r in rows),
        "cost_usd": cost or None,  # OAuth sessions report 0: unknown, not free
    }


class SynapsAgent(BaseInstalledAgent):
    # skills: a task's skills_dir is copied into ~/.synaps-cli/skills (synaps
    # loads <name>/SKILL.md there), as Claude Code's adapter copies it into its
    # own skills dir.
    # mcp_servers: synaps 0.11.1 spawns stdio servers only, so an SSE server is
    # registered as `mcp-sse-bridge <url>` (static stdio<->SSE bridge).
    capabilities = AgentCapabilities(atif=True, reasoning_effort=True, skills=True,
                                     mcp_servers=True)
    options_model = SynapsOptions
    options: SynapsOptions

    @staticmethod
    @override
    def name() -> str:
        return "synaps"

    @override
    def get_version_command(self) -> str | None:
        return f"{INSTALL_DIR}/synaps --version"

    @override
    def parse_version(self, stdout: str) -> str:
        m = re.search(r"(\d+\.\d+\.\d+)", stdout)
        return m.group(1) if m else stdout.strip()

    # ── model / auth ────────────────────────────────────────────────────

    def _synaps_model(self) -> str:
        model = self.model_name or "anthropic/claude-opus-5-5"
        return model if "/" in model else f"anthropic/{model}"

    def _effort(self) -> str:
        return self.options.reasoning_effort or "xhigh"

    def _broker_config(self) -> str:
        """Config lines that make synaps fetch tokens from the host broker.

        The broker is the only refresher (Anthropic rotates the refresh token on
        every refresh), so long trials never outlive their token and nothing a
        container does can log the host's own sessions out.
        """
        url = os.environ.get("SYNAPS_BROKER_URL", "http://172.17.0.1:8181").rstrip("/")
        token_file = Path(
            os.environ.get("SYNAPS_BROKER_TOKEN_FILE", HERE / "broker" / "machine-token")
        ).expanduser()
        token = token_file.read_text().strip()
        if not token:
            raise RuntimeError(f"empty machine token: {token_file}")
        return f"auth.remote_endpoint = {url}\nauth.machine_token = {token}\n"

    async def _upload_text(
        self, environment: BaseEnvironment, text: str, target: str, mode: int = 0o600
    ) -> None:
        # Through a temp file: transfers are not logged as commands, so the
        # machine token never lands in trial logs.
        fd, tmp = tempfile.mkstemp(prefix="synaps-up-")
        try:
            os.fchmod(fd, mode)
            with os.fdopen(fd, "w") as f:
                f.write(text)
            await environment.upload_file(tmp, target)
        finally:
            os.unlink(tmp)

    # ── lifecycle ───────────────────────────────────────────────────────

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        binary = Path(os.environ.get("SYNAPS_BINARY", DEFAULT_BINARY)).expanduser()
        if not binary.is_file():
            raise RuntimeError(f"synaps binary not found: {binary}")
        relay = Path(os.environ.get("SYNAPS_UDS_RELAY", DEFAULT_RELAY)).expanduser()
        if not relay.is_file():
            raise RuntimeError(f"uds-relay binary not found: {relay}")
        await environment.upload_file(str(binary), f"{INSTALL_DIR}/synaps")
        await environment.upload_file(str(relay), f"{INSTALL_DIR}/uds-relay")
        # Not "*run.sh": vba-userform-port's agent ran `pkill run.sh` and killed its own driver.
        await environment.upload_file(str(DRIVER), f"{INSTALL_DIR}/sx-driver")
        await self.exec_as_root(
            environment,
            command=(
                f"chmod 0755 {INSTALL_DIR}/synaps {INSTALL_DIR}/uds-relay {INSTALL_DIR}/sx-driver && "
                f"ln -sf {INSTALL_DIR}/synaps /usr/local/bin/synaps && "
                f"ln -sf {INSTALL_DIR}/uds-relay /usr/local/bin/uds-relay && "
                "command -v bash >/dev/null && synaps --version"
            ),
        )
        config = (
            f"model = {self._synaps_model()}\n"
            f"thinking = {self._effort()}\n"
            "telemetry = full\n"  # local-only api-log + request trace, no request content
        ) + self._broker_config()
        # docker cp creates root-owned files: stage root-only, then install into
        # the agent user's home with that user as owner.
        who = await self.exec_as_agent(
            environment, command='printf "%s %s %s" "$(id -u)" "$(id -g)" "$HOME"'
        )
        uid, gid, home = (who.stdout or "").strip().split(" ", 2)
        await self._upload_text(environment, config, f"{INSTALL_DIR}/config")
        await self._upload_text(environment, SYSTEM_PROMPT, f"{INSTALL_DIR}/system.md", 0o644)
        await self.exec_as_root(
            environment,
            command=(
                f"set -e; d={home}/.synaps-cli; "
                f'install -d -o {uid} -g {gid} -m 0700 "$d"; '
                f'install -o {uid} -g {gid} -m 0600 {INSTALL_DIR}/config "$d/config"; '
                f'install -o {uid} -g {gid} -m 0644 {INSTALL_DIR}/system.md "$d/system.md"; '
                f"rm -f {INSTALL_DIR}/config"
            ),
        )
        if self.mcp_servers:
            await self._install_mcp(environment, home, uid, gid)
        if self.skills_dir:
            await self.exec_as_agent(
                environment,
                command=(
                    'mkdir -p "$HOME/.synaps-cli/skills" && '
                    f"(cp -r {shlex.quote(self.skills_dir)}/* \"$HOME/.synaps-cli/skills/\" 2>/dev/null || true) && "
                    'ls "$HOME/.synaps-cli/skills"'
                ),
            )

    def _mcp_config(self) -> dict[str, Any]:
        servers: dict[str, Any] = {}
        for srv in self.mcp_servers:
            if srv.transport == "stdio":
                servers[srv.name] = {"command": srv.command, "args": list(srv.args)}
            elif srv.transport == "sse" and (srv.url or "").startswith("http://") and not srv.headers:
                servers[srv.name] = {"command": f"{INSTALL_DIR}/mcp-sse-bridge", "args": [srv.url]}
            else:
                # Fail the trial loudly rather than run it without a tool the
                # task provides.
                raise RuntimeError(f"unsupported MCP server for synaps: {srv.name} ({srv.transport} {srv.url})")
        return {"mcpServers": servers}

    async def _install_mcp(self, environment: BaseEnvironment, home: str, uid: str, gid: str) -> None:
        config = self._mcp_config()
        if any(c["command"].endswith("mcp-sse-bridge") for c in config["mcpServers"].values()):
            bridge = Path(os.environ.get("SYNAPS_MCP_BRIDGE", DEFAULT_MCP_BRIDGE)).expanduser()
            if not bridge.is_file():
                raise RuntimeError(f"mcp-sse-bridge binary not found: {bridge}")
            await environment.upload_file(str(bridge), f"{INSTALL_DIR}/mcp-sse-bridge")
            await self.exec_as_root(environment, command=f"chmod 0755 {INSTALL_DIR}/mcp-sse-bridge")
        await self._upload_text(environment, json.dumps(config, indent=2), f"{INSTALL_DIR}/mcp.json", 0o644)
        await self.exec_as_root(
            environment,
            command=f'install -o {uid} -g {gid} -m 0644 {INSTALL_DIR}/mcp.json "{home}/.synaps-cli/mcp.json"',
        )

    @override
    @with_prompt_template
    async def run(
        self, instruction: str, environment: BaseEnvironment, context: AgentContext
    ) -> None:
        # One JSON string per line; synaps-run.sh embeds each as a Submit's text.
        await self._upload_text(
            environment,
            "".join(json.dumps(p) + "\n" for p in (instruction, CONFIRM)),
            f"{INSTALL_DIR}/prompts.jsonl",
            0o644,
        )
        await self.exec_as_agent(
            environment,
            command=f"bash {INSTALL_DIR}/sx-driver {INSTALL_DIR}/prompts.jsonl {LOG_DIR}",
        )

    @override
    def populate_context_post_run(self, context: AgentContext) -> None:
        traj = self.convert_trajectory(self.logs_dir)
        if traj is None:
            return
        (self.logs_dir / "trajectory.json").write_text(format_trajectory_json(traj.to_json_dict()))
        # Task totals: every api-log row once, subagents included (see convert_trajectory).
        fm = traj.final_metrics
        context.n_input_tokens = fm.total_prompt_tokens
        context.n_cache_tokens = fm.total_cached_tokens
        context.n_output_tokens = fm.total_completion_tokens
        context.cost_usd = fm.total_cost_usd

    # ── ATIF ────────────────────────────────────────────────────────────

    @override
    def convert_trajectory(self, logs_dir: Path) -> Trajectory | None:
        """ATIF from the daemon event log: one user step per turn (its text comes
        from `turn_started`), one agent step per model response of the attached
        session (refused attempts included), metrics from that session's own
        api-log row for the response (see _pair_responses)."""
        events = logs_dir / EVENTS_LOG
        if not events.exists():
            return None
        rows = _read_api_log(logs_dir / API_LOG)

        items: list[dict[str, Any]] = []  # user turns and model responses, in order
        cur: dict[str, Any] | None = None
        session_id = None
        skipped_frames = 0

        def flush() -> None:
            nonlocal cur
            if cur is not None:
                items.append(cur)
                cur = None

        for line in events.read_text(errors="replace").splitlines():
            try:
                fr = json.loads(line)
            except json.JSONDecodeError:
                continue
            if fr.get("type") == "attached":
                session_id = (fr.get("meta") or {}).get("id")
                continue
            if fr.get("type") != "event":
                continue
            if session_id and fr.get("session_id") not in (None, session_id):
                skipped_frames += 1  # another session's frame
                continue
            ev = fr.get("event") or {}
            kind = ev.get("ev")
            if kind == "turn_started":
                flush()
                items.append({"user": ev.get("user_text") or f"[{ev.get('trigger')} turn]",
                              "ts": fr.get("ts")})
                continue
            if kind != "stream":
                continue
            inner = ev.get("event") or {}
            if inner.get("kind") == "session":
                if inner.get("session") in ("done", "error"):
                    flush()
                elif inner.get("session") == "usage" and cur is not None and cur["usage"] is None:
                    cur["usage"] = _event_usage(inner)
                continue
            if inner.get("kind") != "llm":
                continue
            et = inner.get("llm")
            if et == "response_start":
                flush()
                cur = {"ts": fr.get("ts"), "text": [], "think": [], "calls": {}, "usage": None}
            elif cur is None:
                continue
            elif et == "text":
                cur["text"].append(inner.get("text") or "")
            elif et == "thinking":
                cur["think"].append(inner.get("text") or inner.get("thinking") or "")
            elif et == "tool_use_start":
                cur["calls"].setdefault(inner["tool_id"], {"name": inner.get("tool_name") or "?",
                                                           "args": {}, "result": None})
            elif et == "tool_use":
                c = cur["calls"].setdefault(inner["tool_id"], {"name": "?", "args": {}, "result": None})
                c["name"] = inner.get("tool_name") or c["name"]
                inp = inner.get("input")
                c["args"] = inp if isinstance(inp, dict) else {"input": inp}
            elif et == "tool_result":
                c = cur["calls"].setdefault(inner["tool_id"], {"name": "?", "args": {}, "result": None})
                r = inner.get("result")
                c["result"] = r if isinstance(r, str) else json.dumps(r)
        flush()
        if not items:
            return None

        parent, subagent, other = _split_rows(session_id, rows or [])
        responses = [it for it in items if "user" not in it]
        paired, attribution = _pair_responses(session_id, rows, parent, [r["usage"] for r in responses])
        if skipped_frames:
            attribution["other_session_frames"] = skipped_frames
        req_of = {id(r): q for r, q in zip(responses, paired)}

        steps: list[Step] = [Step(step_id=1, source="system", message=SYSTEM_PROMPT)]
        for it in items:
            if "user" in it:
                steps.append(Step(step_id=len(steps) + 1, source="user", message=it["user"],
                                  timestamp=it["ts"]))
                continue
            req = req_of[id(it)]
            calls = [ToolCall(tool_call_id=i, function_name=c["name"], arguments=c["args"])
                     for i, c in it["calls"].items()]
            results = [ObservationResult(source_call_id=i, content=c["result"])
                       for i, c in it["calls"].items() if c["result"] is not None]
            metrics = None
            if req:
                u = req["usage"]
                metrics = Metrics(
                    prompt_tokens=_prompt_tokens(u),
                    completion_tokens=u.get("output"),
                    cached_tokens=u.get("cache_read"),
                    cost_usd=req.get("cost_usd") or None,
                    extra={"cache_write_tokens": u.get("cache_write"),
                           "stop_reason": req.get("stop_reason")},
                )
            steps.append(Step(
                step_id=len(steps) + 1, source="agent",
                timestamp=_now_iso(req.get("ts")) if req else it["ts"],
                model_name=req.get("model") if req else None,
                message="".join(it["text"]),
                reasoning_content="".join(it["think"]) or None,
                tool_calls=calls or None,
                observation=Observation(results=results) if results else None,
                metrics=metrics, llm_call_count=1,
            ))

        # Totals are the task's: every api-log row exactly once, the attached
        # session's and its subagents' (subagent work is part of the task's cost).
        # Step metrics cover the attached session only, so with subagents the
        # totals exceed the sum over steps; `extra` has the split.
        total = _tally(rows or [])
        extra: dict[str, Any] = {
            "attribution": attribution,
            "api_requests": total["requests"],
            "parent": {"session_id": session_id, "runtime_session_ids": _runtime_ids(parent),
                       **_tally(parent)},
            "subagents": {"runtime_session_ids": _runtime_ids(subagent), **_tally(subagent)},
        }
        if other:
            extra["unattributed"] = {"runtime_session_ids": _runtime_ids(other), **_tally(other)}
        return Trajectory(
            schema_version="ATIF-v1.7",
            session_id=session_id,
            agent=Agent(name=self.name(), version=self.version() or "unknown",
                        model_name=self._synaps_model(),
                        extra={"thinking": self._effort(), "transport": "synaps daemon"}),
            steps=steps,
            final_metrics=FinalMetrics(
                total_prompt_tokens=total["prompt_tokens"],
                total_completion_tokens=total["completion_tokens"],
                total_cached_tokens=total["cached_tokens"],
                total_cost_usd=total["cost_usd"],
                total_steps=len(steps),
                extra=extra,
            ),
        )
