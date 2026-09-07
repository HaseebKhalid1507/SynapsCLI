#!/usr/bin/env python3
"""Explicit autonomous session-driver policy. Python stdlib; stdout is RPC only."""

from collections import OrderedDict
from contextlib import contextmanager
from copy import deepcopy
from dataclasses import dataclass
import json
import os
from pathlib import Path
import re
import stat
import sys
import time
from typing import Optional
import uuid

MAX_FRAME = 64 * 1024
MAX_HEADER = 4096
MAX_HEADER_LINE = 1024
MAX_DEPTH = 32
MAX_PREFS = 8192
MAX_GOAL = 12 * 1024
MAX_TURNS = 1_000_000
MAX_DURATION_MS = 365 * 24 * 60 * 60 * 1000
MIN_DELAY_MS = 1000
COOLDOWN_MS = 300_000
RETRY_DELAYS = (2000, 4000, 8000)
REPEAT_THRESHOLD = 3
FEEDBACKS = frozenset({"unknown", "changed", "repeated", "empty"})
CACHE_SIZE = 256
EFFORTS = frozenset("off adaptive low medium high xhigh max ultra ultracode".split())
DEFAULT_FAVORITES = (
    ("openai-codex/gpt-6-astra", "ultra"),
    ("anthropic/claude-fable-5-1", "xhigh"),
    ("kimi-code/k3", "max"),
    ("x-ai/grok-4.6", "high"),
)
MODEL_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*(?:/[A-Za-z0-9][A-Za-z0-9._:-]*)+")
ID_RE = re.compile(r"[A-Za-z0-9_-]{1,128}")
USAGE = "/auto start [--turns N] [--for Nm|Nh|Nd] [--context auto|off] -- <goal>; stop; status; favorites [set <model> <effort> ...|reset]"


class Invalid(ValueError):
    pass


class FrameError(ValueError):
    pass


def text(value, limit, label, empty=False):
    if not isinstance(value, str):
        raise Invalid(label + " must be text")
    try:
        size = len(value.encode("utf-8"))
    except UnicodeError:
        raise Invalid(label + " must be valid Unicode") from None
    if size > limit or (not empty and not value.strip()):
        raise Invalid(label + " is empty or too large")
    if any(ord(c) < 32 and c not in "\t\n\r" or ord(c) == 127 for c in value):
        raise Invalid(label + " contains control characters")
    return value


def defaults():
    return [{"model": model, "effort": effort} for model, effort in DEFAULT_FAVORITES]


def favorites(value):
    if not isinstance(value, list) or not 1 <= len(value) <= 16:
        raise Invalid("favorites must contain 1..16 model/effort pairs")
    seen = set()
    for entry in value:
        if not isinstance(entry, dict) or set(entry) != {"model", "effort"}:
            raise Invalid("each favorite must contain exactly model and effort")
        model = text(entry["model"], 256, "model")
        effort = text(entry["effort"], 32, "effort")
        if not MODEL_RE.fullmatch(model):
            raise Invalid("model must be an exact qualified provider/model identifier")
        if model in seen:
            raise Invalid("duplicate favorite model")
        if effort not in EFFORTS:
            raise Invalid("unknown effort; use a canonical name, without aliases")
        seen.add(model)
    return deepcopy(value)


def json_bytes(value):
    return json.dumps(value, ensure_ascii=False, allow_nan=False, separators=(",", ":")).encode("utf-8")


def strict_json(data):
    """Bound size/depth before decoding; reject duplicate keys and non-JSON numbers."""
    if len(data) > MAX_FRAME:
        raise Invalid("JSON too large")
    if isinstance(data, bytes):
        data = data.decode("utf-8", errors="strict")
    depth, quoted, escaped = 0, False, False
    for char in data:
        if quoted:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                quoted = False
        elif char == '"':
            quoted = True
        elif char in "[{":
            depth += 1
            if depth > MAX_DEPTH:
                raise Invalid("JSON nesting too deep")
        elif char in "]}":
            depth -= 1

    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise Invalid("duplicate JSON key")
            result[key] = value
        return result

    def integer(raw):
        if len(raw) > 20:
            raise Invalid("JSON integer too large")
        return int(raw)

    def no_constant(_):
        raise Invalid("non-finite JSON number")

    return json.loads(data, object_pairs_hook=pairs, parse_int=integer,
                      parse_float=no_constant, parse_constant=no_constant)


class Preferences:
    """One private plugin-local file; no environment or host configuration writes."""

    def __init__(self, root):
        self.root = os.path.abspath(root)

    @contextmanager
    def directory(self):
        if not all(hasattr(os, name) for name in ("O_NOFOLLOW", "O_DIRECTORY", "getuid")):
            raise Invalid("private preferences require POSIX no-follow filesystem support")
        # Pin the directory for every operation, refusing even ancestor symlinks.
        fd = os.open("/", os.O_RDONLY | os.O_DIRECTORY)
        try:
            for part in Path(self.root).parts[1:]:
                child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
                os.close(fd)
                fd = child
            info = os.fstat(fd)
            if info.st_uid != os.getuid() or info.st_mode & 0o022:
                raise Invalid("plugin directory must be owned by you and not group/world writable")
            yield fd
        finally:
            os.close(fd)

    @staticmethod
    def check_file(info):
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                or info.st_nlink != 1 or stat.S_IMODE(info.st_mode) != 0o600):
            raise Invalid("prefs.json must be a private owned regular file (0600), not a link")
        if info.st_size > MAX_PREFS:
            raise Invalid("prefs.json too large")

    def existing(self, fd):
        try:
            info = os.stat("prefs.json", dir_fd=fd, follow_symlinks=False)
        except FileNotFoundError:
            return
        self.check_file(info)

    def load(self):
        with self.directory() as directory:
            self.existing(directory)
            try:
                fd = os.open("prefs.json", os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK,
                             dir_fd=directory)
            except FileNotFoundError:
                return defaults()
            with os.fdopen(fd, "rb") as stream:
                self.check_file(os.fstat(stream.fileno()))
                data = stream.read(MAX_PREFS + 1)
        if len(data) > MAX_PREFS:
            raise Invalid("prefs.json too large")
        value = strict_json(data)
        if (not isinstance(value, dict) or set(value) != {"version", "favorites"}
                or type(value["version"]) is not int or value["version"] != 1):
            raise Invalid("prefs.json schema must be version 1 with favorites only")
        return favorites(value["favorites"])

    def save(self, choices):
        data = json_bytes({"version": 1, "favorites": favorites(choices)})
        if len(data) > MAX_PREFS:
            raise Invalid("preferences too large")
        with self.directory() as directory:
            self.existing(directory)
            name = ".prefs-" + uuid.uuid4().hex + ".tmp"
            fd = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                         0o600, dir_fd=directory)
            try:
                with os.fdopen(fd, "wb") as stream:
                    os.fchmod(stream.fileno(), 0o600)
                    stream.write(data)
                    stream.flush()
                    os.fsync(stream.fileno())
                self.existing(directory)
                os.replace(name, "prefs.json", src_dir_fd=directory, dst_dir_fd=directory)
                os.fsync(directory)
            finally:
                try:
                    os.unlink(name, dir_fd=directory)
                except FileNotFoundError:
                    pass


def parse_start(args):
    turns = duration = None
    context_mode = "auto"
    index = 0
    seen = set()
    while index < len(args) and args[index] != "--":
        flag = args[index]
        if (flag not in ("--turns", "--for", "--context") or flag in seen
                or index + 1 >= len(args) or args[index + 1].startswith("--")):
            raise Invalid("invalid, repeated or missing-value start flag; " + USAGE)
        seen.add(flag)
        raw = args[index + 1]
        if flag == "--context":
            if raw not in ("auto", "off"):
                raise Invalid("--context must be auto or off")
            context_mode = raw
        elif flag == "--turns":
            if not re.fullmatch(r"[1-9][0-9]{0,6}", raw) or int(raw) > MAX_TURNS:
                raise Invalid("--turns must be an integer in 1..1000000")
            turns = int(raw)
        else:
            match = re.fullmatch(r"([1-9][0-9]{0,8})([mhd])", raw)
            if not match:
                raise Invalid("--for requires a positive integer followed by m, h or d")
            duration = int(match[1]) * {"m": 60_000, "h": 3_600_000, "d": 86_400_000}[match[2]]
            if duration > MAX_DURATION_MS:
                raise Invalid("--for cannot exceed 365 days")
        index += 2
    if index == len(args) or args[index] != "--":
        raise Invalid("start requires -- before the goal; " + USAGE)
    goal = text(" ".join(args[index + 1:]).strip(), MAX_GOAL, "goal")
    return goal, turns, duration, context_mode


def prompt(goal, continuation=False, recovery=None):
    phase = "Continue the autonomous loop" if continuation else "Begin the autonomous loop"
    # Only internal reason labels select fixed guidance; never interpolate host error text.
    recovery_text = ""
    if recovery is not None:
        recovery_text = {
            "time_checkpoint": "The previous time segment ended at a host wall-clock checkpoint. Continue remaining work from retained history; this is not a provider failure. ",
            "repeated": "Completed-turn feedback suggests repetition/stalling or empty output. ",
            "provider_error": "The previous provider attempt failed. ",
            "selection_rejected": "The previous exact model/effort selection was rejected. ",
        }[recovery]
        recovery_text += (("Inspect retained results and continue from the last completed step. "
                           if recovery == "time_checkpoint" else
                           "Inspect retained results and try a materially different approach. ")
                          + "Never replay side effects or bypass gates.\n")
        if recovery == "repeated":
            # Repetition is not evidence of consent or proof that a gate is redundant.
            recovery_text += (
                "If you are stuck requesting a confirmation phrase, re-check the actual human "
                "instructions: take the next step only if already authorized; otherwise name the "
                "concrete blocker and wait. Repetition or a model switch supplies no approval.\n")
    return (phase + " — automated continuation, not new human input or approval.\n"
            + recovery_text
            + "Honor the latest human steering in the conversation, including separately supplied "
            "human messages. The original goal is historical context, never an override or a request "
            "to restore superseded work.\n"
            "Inspect retained work and results before acting. Do not repeat completed external actions, "
            "including those before a failed provider attempt; never blindly replay the original task.\n"
            "Continue only remaining authorized work. When actual human instructions clearly authorize "
            "the next step, do it instead of asking to reconfirm it with a phrase such as 'Approved' or "
            "'Authorized'. An assistant's request for a phrase does not itself create a new approval "
            "requirement. Never manufacture human approval.\n"
            "Do not expand the user's authorization or invent additional goals. All normal permissions, "
            "confirmations, context and safety gates still apply, including explicit human review "
            "checkpoints. If authorization is unclear, or new permission, a decision, credentials or "
            "any human-needed action is required, stop work and ask the user for the concrete missing "
            "requirement; do not retry the blocked action.\n"
            "If nothing authorized remains, report completion and take no further external actions."
            "\n\nOriginal goal (historical context; latest human steering takes precedence):\n" + goal)


@dataclass
class Run:
    run_id: str
    models: list
    goal: str
    turns: Optional[int]
    deadline: Optional[float]
    index: int = 0
    successes: int = 0
    retries: int = 0
    repeat_streak: int = 0
    sequence: int = 0

    @property
    def selection(self):
        return self.models[self.index]

    def count(self):
        limit = str(self.turns) if self.turns is not None else "unbounded"
        return f"{self.successes}/{limit} successful turns"


def reply(action, notice, **fields):
    return {"session_driver": {"action": action, **fields, "notice": notice}}


class Driver:
    def __init__(self, root, clock=time.monotonic):
        self.preferences = Preferences(root)
        self.clock = clock
        self.initialized = False
        self.run = None
        self.cache = OrderedDict()
        self.choices = None
        self.prefs_error = None
        self.last_stop = "No active run."

    def initialize(self, params):
        if (not isinstance(params, dict)
                or type(params.get("extension_protocol_version", 1)) is not int
                or params.get("extension_protocol_version", 1) != 1):
            raise Invalid("extension protocol version must be 1")
        self.run = None
        self.cache.clear()
        self.last_stop = "No active run; initialization/restart never resumes a run."
        try:
            self.choices = self.preferences.load()
            self.prefs_error = None
        except (OSError, ValueError, UnicodeError):
            self.choices = None
            self.prefs_error = "Preferences unavailable/unsafe; inspect plugin-local prefs.json or use favorites set/reset."
        self.initialized = True
        return {"protocol_version": 1, "capabilities": {"tools": [], "providers": []}}

    def stop(self, reason):
        if self.run is not None:
            reason += " " + self.run.count() + "."
        self.run = None
        self.last_stop = reason
        return reply("stop", reason)

    def expired(self):
        return self.run is not None and self.run.deadline is not None and self.clock() >= self.run.deadline

    def status(self):
        if self.expired():
            self.stop("Duration limit reached.")
        if self.run is None:
            notice = self.last_stop
        else:
            run = self.run
            remaining = ("no duration limit" if run.deadline is None else
                         f"{max(0, int((run.deadline - self.clock()) * 1000))}ms remaining")
            notice = (f"Plugin run {run.run_id}: {run.count()}; "
                      f"{run.selection['model']} {run.selection['effort']}; {remaining}. "
                      "Host grant may already be revoked; only the TUI knows its live grant.")
        if self.prefs_error:
            notice += " " + self.prefs_error
        return reply("status", notice)

    def command(self, args):
        if not self.initialized:
            raise Invalid("initialize first")
        if (not isinstance(args, list) or len(args) > 4096
                or any(not isinstance(arg, str) for arg in args)):
            raise Invalid("args must be a bounded list of strings")
        text(" ".join(args), 32 * 1024, "command", empty=True)
        if args == ["status"]:
            return self.status(), None
        if args == ["stop"]:
            return self.stop("Stopped by user."), None
        if args and args[0] == "start":
            goal, turns, duration, context_mode = parse_start(args[1:])
            if self.choices is None:
                raise Invalid(self.prefs_error)
            # Explicit start replaces stale plugin bookkeeping, never a host grant by itself.
            self.run = Run(str(uuid.uuid4()), deepcopy(self.choices), goal, turns,
                           None if duration is None else self.clock() + duration / 1000)
            self.cache.clear()
            run = self.run
            context_notice = (
                "Auto enables automatic context continuation and local archival of eligible session content. "
                if context_mode == "auto" else
                "Off disables automatic context continuation; existing archives are not deleted. ")
            notice = (f"Autonomous run proposed: {run.count()}. May incur ongoing charges and "
                      "send retained conversation history across favorite providers. "
                      f"Context mode: {context_mode} (foreground session only), applied only after host "
                      "grant acceptance and the same validation as /context auto/off. "
                      + context_notice
                      + "Local archives exclude system/developer prompts, private reasoning, "
                      "restricted/sensitive content and binary blocks; redaction is not a universal secret detector. "
                      "The mode persists in the current session after the run stops, like explicit /context. "
                      "No global config or memory recall/capture consent changes; delegated worker context is unchanged. "
                      "Wall-clock checkpoints continue retained work on the same favorite without counting a successful turn; the run deadline remains in force. "
                      "Exact favorites only; normal permissions still apply. Stop with Esc or /auto stop.")
            fields = dict(run_id=run.run_id, models=deepcopy(run.models), prompt=prompt(goal),
                          delay_ms=MIN_DELAY_MS, feedback_version=1, time_checkpoint_version=1, context_mode=context_mode)
            if duration is not None:
                fields["max_duration_ms"] = duration
            return reply("start", notice, **fields), None
        if args and args[0] == "favorites":
            if args == ["favorites"]:
                if self.choices is None:
                    raise Invalid(self.prefs_error)
            else:
                if args == ["favorites", "reset"]:
                    choices = defaults()
                elif len(args) >= 4 and args[1] == "set" and len(args[2:]) % 2 == 0:
                    choices = favorites([{"model": args[i], "effort": args[i + 1]}
                                         for i in range(2, len(args), 2)])
                else:
                    raise Invalid("invalid favorites command; " + USAGE)
                try:
                    self.preferences.save(choices)
                except (OSError, ValueError):
                    raise Invalid("Could not save private prefs.json; no in-memory favorites changed.") from None
                self.choices, self.prefs_error = choices, None
            rows = [[str(i), entry["model"], entry["effort"]]
                    for i, entry in enumerate(self.choices, 1)]
            return reply("status", "Favorites in order; changes apply only to the next explicit start."), rows
        raise Invalid(USAGE)

    def poll(self, args):
        if self.run is None:
            return reply("stop", self.last_stop)
        try:
            if not isinstance(args, list) or len(args) != 1:
                raise Invalid("one JSON poll argument required")
            request = strict_json(text(args[0], 4096, "poll"))
            keys = {"run_id", "decision_id", "outcome", "error_kind", "model", "effort"}
            if not isinstance(request, dict) or set(request) not in (keys, keys | {"feedback"}):
                raise Invalid("invalid poll fields")
            if any(not isinstance(value, str) for value in request.values()):
                raise Invalid("poll fields must be strings")
            if not ID_RE.fullmatch(request["run_id"]) or request["run_id"] != self.run.run_id:
                raise Invalid("run id mismatch")
            decision = request["decision_id"]
            if not ID_RE.fullmatch(decision):
                raise Invalid("decision_id must be 1..128 ASCII letters/digits/-/_")
            if request["outcome"] not in {"success", "provider_error", "selection_rejected", "time_checkpoint", "blocked"}:
                raise Invalid("unknown outcome")
            if request["error_kind"] not in {"", "none", "auth", "quota", "rate_limit", "transient", "wall_clock", "unknown"}:
                raise Invalid("unknown error kind")
            if ((request["outcome"] == "time_checkpoint")
                    != (request["error_kind"] == "wall_clock")):
                raise Invalid("inconsistent time checkpoint")
            feedback = request.get("feedback", "unknown")
            if feedback not in FEEDBACKS:
                raise Invalid("unknown feedback")
            if request["outcome"] != "success" and feedback != "unknown":
                raise Invalid("feedback requires a successful completed turn")
        except (ValueError, UnicodeError):
            return self.stop("Malformed or mismatched driver callback; stopped safely.")
        if self.expired():
            return self.stop("Duration limit reached.")
        run = self.run
        # Preserve optional-field presence as well as value; do not normalize missing feedback.
        fingerprint = tuple(request[key] for key in sorted(request))
        if decision in self.cache:
            previous, sequence, result = self.cache[decision]
            self.cache.move_to_end(decision)
            if previous != fingerprint or sequence != run.sequence:
                return self.stop("Repeated decision changed payload or is stale; stopped safely.")
            # A transport retry may replay only the latest identical decision, never account it twice.
            if not self.can_delay(result["session_driver"].get("delay_ms", 0)):
                return self.stop("Duration limit would expire during delay.")
            return deepcopy(result)
        if {"model": request["model"], "effort": request["effort"]} != run.selection:
            return self.stop("Callback model/effort does not match last emitted selection.")
        outcome, kind = request["outcome"], request["error_kind"]
        if outcome != "success":
            # A failed attempt (including a zero-send rejection) breaks consecutive successes.
            run.repeat_streak = 0
        if outcome == "blocked":
            result = self.stop("Host blocked the run; human attention required.")
        elif outcome == "time_checkpoint":
            if kind != "wall_clock":
                result = self.stop("Inconsistent time checkpoint; stopped safely.")
            elif not self.can_delay(MIN_DELAY_MS):
                result = self.stop("Duration limit would expire during delay.")
            else:
                run.retries = 0
                result = self.next(MIN_DELAY_MS,
                                   "Wall-clock checkpoint: continuing retained work on the same favorite.",
                                   "time_checkpoint")
        elif outcome == "success":
            if kind not in ("", "none", "unknown"):
                result = self.stop("Inconsistent success/error callback; stopped safely.")
            else:
                run.successes += 1
                run.retries = 0
                # Limits win over feedback recovery, including the normal submission delay.
                if run.turns is not None and run.successes >= run.turns:
                    result = self.stop("Successful turn limit reached.")
                elif not self.can_delay(MIN_DELAY_MS):
                    result = self.stop("Duration limit would expire during delay.")
                else:
                    run.repeat_streak = (run.repeat_streak + 1
                                         if feedback in {"repeated", "empty"} else 0)
                    if run.repeat_streak >= REPEAT_THRESHOLD:
                        result = self.advance("Completed-turn feedback repeated/empty 3 times",
                                              "repeated")
                    elif run.repeat_streak:
                        # Give the current favorite a bounded correction before failover.
                        # Only successful-turn enum feedback reaches this branch, never a
                        # blocked outcome or a guessed approval classification from prose.
                        result = self.next(MIN_DELAY_MS,
                                           "Repeated/empty completed turn: correcting course on the same favorite.",
                                           "repeated")
                    else:
                        result = self.next(MIN_DELAY_MS, "Continuing authorized remaining work.")
        elif outcome == "provider_error" and kind not in {"auth", "quota", "rate_limit", "transient"}:
            result = self.stop("Unknown provider error; human attention required.")
        elif outcome == "provider_error" and kind in {"transient", "rate_limit"} and run.retries < len(RETRY_DELAYS):
            delay = RETRY_DELAYS[run.retries]
            run.retries += 1
            result = self.next(delay, f"{kind}: retry {run.retries}/{len(RETRY_DELAYS)} of exact favorite.",
                               "provider_error")
        else:
            result = self.advance(f"{outcome}/{kind}", outcome)
        run.sequence += 1
        self.cache[decision] = (fingerprint, run.sequence, deepcopy(result))
        self.cache.move_to_end(decision)
        while len(self.cache) > CACHE_SIZE:
            self.cache.popitem(last=False)
        return result

    def can_delay(self, delay_ms):
        return (self.run.deadline is None
                or self.clock() + delay_ms / 1000 < self.run.deadline)

    def advance(self, reason, recovery):
        run = self.run
        index = (run.index + 1) % len(run.models)
        delay = COOLDOWN_MS if index == 0 else MIN_DELAY_MS
        if not self.can_delay(delay):
            return self.stop("Duration limit would expire during delay.")
        previous = f"{run.selection['model']} {run.selection['effort']}"
        run.index = index
        run.retries = 0
        run.repeat_streak = 0
        selected = f"{run.selection['model']} {run.selection['effort']}"
        notice = f"{reason}: skipped {previous}; next exact favorite: {selected}."
        if index == 0:
            notice += " All favorites exhausted; cooldown 5 minutes, then first favorite."
        return self.next(delay, notice, recovery)

    def next(self, delay, notice, recovery=None):
        if not self.can_delay(delay):
            return self.stop("Duration limit would expire during delay.")
        run = self.run
        return reply("next", notice + " " + run.count() + ".", run_id=run.run_id,
                     selection=deepcopy(run.selection), prompt=prompt(run.goal, True, recovery),
                     delay_ms=delay)


def read_message(stream):
    """Bounded byte-exact Content-Length framing; ambiguous framing terminates transport."""
    length, total = None, 0
    while True:
        line = stream.readline(MAX_HEADER_LINE + 1)
        if not line and total == 0:
            return None
        total += len(line)
        if (not line or len(line) > MAX_HEADER_LINE or total > MAX_HEADER
                or not line.endswith(b"\n")):
            raise FrameError("truncated or oversized header")
        if line in (b"\r\n", b"\n"):
            break
        try:
            header = line.decode("ascii").rstrip("\r\n")
        except UnicodeError:
            raise FrameError("non-ASCII header") from None
        name, sep, value = header.partition(":")
        if not sep or not re.fullmatch(r"[A-Za-z][A-Za-z0-9-]*", name):
            raise FrameError("invalid header")
        value = value.strip(" \t")
        if any(ord(c) < 32 or ord(c) == 127 for c in value):
            raise FrameError("invalid header value")
        if name.lower() == "content-length":
            if length is not None or not re.fullmatch(r"[0-9]{1,6}", value):
                raise FrameError("invalid/duplicate Content-Length")
            length = int(value)
    if length is None or not 0 < length <= MAX_FRAME:
        raise FrameError("missing or oversized Content-Length")
    body = bytearray()
    while len(body) < length:
        chunk = stream.read(length - len(body))
        if not chunk:
            raise FrameError("truncated body")
        body.extend(chunk)
    try:
        return strict_json(bytes(body))
    except (ValueError, UnicodeError):
        raise FrameError("invalid JSON body") from None


def write_message(stream, message):
    body = json_bytes(message)
    if len(body) > MAX_FRAME:
        raise FrameError("response too large")
    stream.write(f"Content-Length: {len(body)}\r\n\r\n".encode("ascii") + body)
    stream.flush()


def serve(input_stream, output_stream, root):
    driver = Driver(root)
    while True:
        request = read_message(input_stream)
        if request is None:
            return
        valid_id = (isinstance(request, dict) and
                    (request.get("id") is None
                     or type(request.get("id")) is int and -(2**63) <= request["id"] < 2**64
                     or isinstance(request.get("id"), str) and bool(ID_RE.fullmatch(request["id"]))))
        if (not isinstance(request, dict) or request.get("jsonrpc") != "2.0"
                or not isinstance(request.get("method"), str) or not valid_id
                or set(request) - {"jsonrpc", "id", "method", "params"}):
            driver.stop("Invalid RPC request; stopped safely.")
            write_message(output_stream, {"jsonrpc": "2.0", "id": None,
                          "error": {"code": -32600, "message": "Invalid Request"}})
            continue
        # Notifications cannot initialize, invoke commands, stop, or mutate policy.
        if "id" not in request:
            continue
        rid, method, params = request["id"], request["method"], request.get("params", {})
        response = {"jsonrpc": "2.0", "id": rid}
        try:
            if method == "initialize":
                result = driver.initialize(params)
            elif method == "shutdown":
                driver.stop("Plugin shut down.")
                result = None
            elif method == "hook.handle":
                result = {"action": "continue"}
            elif method == "info.get":
                result = {"capabilities": [{"kind": "command", "name": "auto",
                                           "modes": ["interactive"]}]}
            elif method == "command.invoke":
                if not isinstance(params, dict) or set(params) != {"command", "args", "request_id"}:
                    driver.stop("Malformed command invocation; stopped safely.")
                    raise Invalid("command.invoke requires command, args and request_id")
                if not isinstance(params["request_id"], str) or not ID_RE.fullmatch(params["request_id"]):
                    driver.stop("Malformed request id; stopped safely.")
                    raise Invalid("invalid request_id")
                if params["command"] == "__session_driver__":
                    result = driver.poll(params["args"])
                elif params["command"] == "auto":
                    result, rows = driver.command(params["args"])
                    if rows is not None:
                        for event in ({"kind": "table", "headers": ["Order", "Model", "Effort"], "rows": rows},
                                      {"kind": "done"}):
                            write_message(output_stream, {"jsonrpc": "2.0", "method": "command.output",
                                          "params": {"request_id": params["request_id"], "event": event}})
                else:
                    raise Invalid("unknown command")
            else:
                response["error"] = {"code": -32601, "message": "Method not found"}
            if "error" not in response:
                response["result"] = result
        except Invalid as error:
            response["error"] = {"code": -32602, "message": str(error)}
        write_message(output_stream, response)
        if method == "shutdown":
            return


def main():
    try:
        serve(sys.stdin.buffer, sys.stdout.buffer, Path(__file__).absolute().parent)
    except (FrameError, OSError, UnicodeError):
        # No raw requests/goals/paths on stderr, and no attempted resynchronization.
        sys.stderr.write("autonomous: invalid or closed transport; run forgotten\n")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
