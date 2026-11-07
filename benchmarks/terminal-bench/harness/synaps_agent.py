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
    then /close (saved) and daemon stop. `--ak completion=closeout` (opt-in,
    not the published run) replaces CONFIRM with follow-ups chosen by how the
    turn ended (see CLOSEOUT and synaps-run.sh),
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
from typing import Any, Literal, Self, override

from pydantic import Field, model_validator

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
COMPLETION_LOG = "completion.jsonl"  # closeout only: one line per decision

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

# completion=closeout (opt-in): no CONFIRM. After each turn synaps-run.sh reads
# how it ended (daemon done/error event, last api-log stop_reason) and sends at
# most: 2 x CONTINUE after an output-limit stop, then one CLOSEOUT after a
# voluntary end; after a refusal nothing, or one REFUSAL_NOTE if enabled. The
# driver fills @E@/@T@/@R@ (elapsed/limit/remaining minutes).
CLOSEOUT_TIME = " Elapsed: @E@ of @T@ minutes (@R@ remaining)."
CLOSEOUT = (
    "Automated close-out (no human is present).{time} Before you stop, check your "
    "work against the task once:\n"
    "1. List each explicit requirement and the evidence you have that it is met, "
    "measured the way the task states it. If a cheap, safe check is missing, run it.\n"
    "2. List any caveat, assumption or risk you raised. For each: if the evidence "
    "shows a requirement is violated, or something contradicts your result (a failing "
    "check, a labelled example, the source the task names as authoritative), fix it "
    "with the smallest justified change, or undo a change you can't justify. "
    "Otherwise leave it and say why it doesn't violate the task. Don't make "
    "speculative changes just to remove a caveat.\n"
    "3. Don't repeat irreversible actions, don't experiment on a live system the task "
    "depends on, and don't weaken a check to make it pass.\n"
    "Then end with a one-line summary."
)
CONTINUE = (
    "Automated message (no human is present): your last response was cut off at the "
    "output limit. Continue from where it stopped. Do long work through tools, and "
    "keep each response shorter."
)
REFUSAL_NOTE = (
    "Automated note (no human is present): your previous response was stopped by the "
    "API's safety classifier. This is not a tool or API error. Its content was withheld "
    "and any tool calls in it did not run. Do not produce that content again, even "
    "reworded. Continue with any other parts of the task that are fine, then end with a "
    "one-line summary."
)

Effort = Literal["low", "medium", "high", "xhigh", "max"]


class SynapsOptions(InstalledAgentOptions):
    reasoning_effort: Effort | None = Field(
        default=None, description="synaps `thinking` level (default: xhigh)."
    )
    completion: Literal["confirm", "closeout"] = Field(
        default="confirm",
        description="After the first turn: confirm = the fixed CONFIRM (published run); "
        "closeout = state-aware continuation/close-out (opt-in).",
    )
    refusal_note: bool = Field(
        default=False,
        description="closeout only: after a refusal send one factual note instead of nothing.",
    )
    agent_timeout_sec: float | None = Field(
        default=None, gt=0,
        description="closeout only: the trial's agent timeout (Harbor doesn't pass it to "
        "agents). Adds elapsed/remaining minutes to the close-out; nothing is sent with "
        "under 60 s left.",
    )

    @model_validator(mode="after")
    def _closeout_flags(self) -> Self:
        if self.completion != "closeout" and (self.refusal_note or self.agent_timeout_sec):
            raise ValueError("refusal_note and agent_timeout_sec need completion=closeout")
        return self


def completion_env(options: SynapsOptions) -> str:
    """completion.env for synaps-run.sh (closeout): shell assignments, messages as
    JSON strings, empty = off/unknown."""
    timeout = options.agent_timeout_sec
    values = {
        "SX_TIMEOUT_S": str(int(timeout)) if timeout else "",
        "SX_CLOSEOUT": json.dumps(CLOSEOUT.format(time=CLOSEOUT_TIME if timeout else "")),
        "SX_CONTINUE": json.dumps(CONTINUE),
        "SX_REFUSAL_NOTE": json.dumps(REFUSAL_NOTE) if options.refusal_note else "",
    }
    return "".join(f"{k}={shlex.quote(v)}\n" for k, v in values.items())


def _now_iso(ms: int | None = None) -> str:
    t = datetime.fromtimestamp(ms / 1000, timezone.utc) if ms else datetime.now(timezone.utc)
    return t.isoformat().replace("+00:00", "Z")


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
        closeout = self.options.completion == "closeout"
        # One JSON string per line; synaps-run.sh embeds each as a Submit's text.
        await self._upload_text(
            environment,
            "".join(json.dumps(p) + "\n" for p in ((instruction,) if closeout else (instruction, CONFIRM))),
            f"{INSTALL_DIR}/prompts.jsonl",
            0o644,
        )
        command = f"bash {INSTALL_DIR}/sx-driver {INSTALL_DIR}/prompts.jsonl {LOG_DIR}"
        if closeout:
            await self._upload_text(environment, completion_env(self.options),
                                    f"{INSTALL_DIR}/completion.env", 0o644)
            command += f" {INSTALL_DIR}/completion.env"
        await self.exec_as_agent(environment, command=command)

    def _completion_meta(self) -> dict[str, Any]:
        """Mode, flags and (closeout) the driver's decisions, so trials from
        different completion modes can't be pooled unnoticed."""
        o = self.options
        if o.completion != "closeout":
            return {"mode": o.completion}
        decisions = []
        log = self.logs_dir / COMPLETION_LOG
        if log.exists():
            for line in log.read_text(errors="replace").splitlines():
                try:
                    decisions.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
        return {"mode": "closeout", "refusal_note": o.refusal_note,
                "agent_timeout_sec": o.agent_timeout_sec, "decisions": decisions}

    @override
    def populate_context_post_run(self, context: AgentContext) -> None:
        completion = self._completion_meta()
        context.metadata = {**(context.metadata or {}), "completion": completion}
        traj = self.convert_trajectory(self.logs_dir)
        if traj is None:
            return
        traj.agent.extra = {**(traj.agent.extra or {}), "completion": completion}
        (self.logs_dir / "trajectory.json").write_text(format_trajectory_json(traj.to_json_dict()))
        fm = traj.final_metrics
        context.n_input_tokens = fm.total_prompt_tokens
        context.n_cache_tokens = fm.total_cached_tokens
        context.n_output_tokens = fm.total_completion_tokens
        context.cost_usd = fm.total_cost_usd

    # ── ATIF ────────────────────────────────────────────────────────────

    @override
    def convert_trajectory(self, logs_dir: Path) -> Trajectory | None:
        """ATIF from the daemon event log: one user step per turn (its text comes
        from `turn_started`), one agent step per model response, metrics from
        the api-log row of that response (same order)."""
        events = logs_dir / EVENTS_LOG
        if not events.exists():
            return None
        requests = []  # one api-log row per model response, in order
        if (logs_dir / API_LOG).exists():
            for line in (logs_dir / API_LOG).read_text(errors="replace").splitlines():
                try:
                    row = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if row.get("usage"):
                    requests.append(row)

        steps: list[Step] = [Step(step_id=1, source="system", message=SYSTEM_PROMPT)]
        cur: dict[str, Any] | None = None
        n_resp = 0
        session_id = None

        def flush() -> None:
            nonlocal cur
            if cur is None:
                return
            calls = [ToolCall(tool_call_id=i, function_name=c["name"], arguments=c["args"])
                     for i, c in cur["calls"].items()]
            results = [ObservationResult(source_call_id=i, content=c["result"])
                       for i, c in cur["calls"].items() if c["result"] is not None]
            req = requests[cur["n"]] if cur["n"] < len(requests) else None
            metrics = None
            if req:
                u = req["usage"]
                metrics = Metrics(
                    prompt_tokens=(u.get("input") or 0) + (u.get("cache_read") or 0)
                    + (u.get("cache_write") or 0),
                    completion_tokens=u.get("output"),
                    cached_tokens=u.get("cache_read"),
                    cost_usd=req.get("cost_usd") or None,
                    extra={"cache_write_tokens": u.get("cache_write"),
                           "stop_reason": req.get("stop_reason")},
                )
            steps.append(Step(
                step_id=len(steps) + 1, source="agent",
                timestamp=_now_iso(req.get("ts")) if req else cur["ts"],
                model_name=req.get("model") if req else None,
                message="".join(cur["text"]),
                reasoning_content="".join(cur["think"]) or None,
                tool_calls=calls or None,
                observation=Observation(results=results) if results else None,
                metrics=metrics, llm_call_count=1,
            ))
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
            ev = fr.get("event") or {}
            kind = ev.get("ev")
            if kind == "turn_started":
                flush()
                steps.append(Step(step_id=len(steps) + 1, source="user",
                                  message=ev.get("user_text") or f"[{ev.get('trigger')} turn]",
                                  timestamp=fr.get("ts")))
                continue
            if kind != "stream":
                continue
            inner = ev.get("event") or {}
            if inner.get("kind") == "session" and inner.get("session") in ("done", "error"):
                flush()
                continue
            if inner.get("kind") != "llm":
                continue
            et = inner.get("llm")
            if et == "response_start":
                flush()
                cur = {"n": n_resp, "ts": fr.get("ts"), "text": [], "think": [], "calls": {}}
                n_resp += 1
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
        if len(steps) == 1:
            return None

        agent_steps = [s for s in steps if s.source == "agent" and s.metrics]
        cost = sum(s.metrics.cost_usd or 0 for s in agent_steps)
        return Trajectory(
            schema_version="ATIF-v1.7",
            session_id=session_id,
            agent=Agent(name=self.name(), version=self.version() or "unknown",
                        model_name=self._synaps_model(),
                        extra={"thinking": self._effort(), "transport": "synaps daemon"}),
            steps=steps,
            final_metrics=FinalMetrics(
                total_prompt_tokens=sum(s.metrics.prompt_tokens or 0 for s in agent_steps),
                total_completion_tokens=sum(s.metrics.completion_tokens or 0 for s in agent_steps),
                total_cached_tokens=sum(s.metrics.cached_tokens or 0 for s in agent_steps),
                total_cost_usd=cost or None,  # OAuth sessions report 0: unknown, not free
                total_steps=len(steps),
            ),
        )
