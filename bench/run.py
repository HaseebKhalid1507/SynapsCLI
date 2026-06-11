#!/usr/bin/env python3
"""
Cache benchmark harness — runs the 21-question suite against the Anthropic
API with a swappable cache breakpoint strategy, capturing per-turn usage.

Auth: ANTHROPIC_API_KEY env var.

Usage:
  python3 run.py --strategy sliding-4          # current SynapsCLI strategy
  python3 run.py --strategy single-last        # Claude Code style
  python3 run.py --strategy last-3             # OpenCode style
  python3 run.py --strategy none               # no message markers (control)
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.request
import urllib.error

from questions import QUESTIONS

API_URL = "https://api.anthropic.com/v1/messages"
DEFAULT_MODEL = "claude-sonnet-4-6"

SYSTEM_PROMPT = (
    "You are a precise coding assistant operating inside a sandbox project "
    "directory. Use the provided tools to complete each task exactly as "
    "specified. Always act via tools - never claim to have done something "
    "without doing it. Keep final answers short."
)

TOOLS = [
    {
        "name": "bash",
        "description": "Run a bash command in the project directory. Returns stdout+stderr.",
        "input_schema": {
            "type": "object",
            "properties": {"command": {"type": "string"}},
            "required": ["command"],
        },
    },
    {
        "name": "read_file",
        "description": "Read a file. Path is relative to the project directory.",
        "input_schema": {
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
        },
    },
    {
        "name": "write_file",
        "description": "Create or overwrite a file. Path relative to project dir. Creates parent dirs.",
        "input_schema": {
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"},
            },
            "required": ["path", "content"],
        },
    },
    {
        "name": "edit_file",
        "description": "Replace an exact string in a file (must match exactly once).",
        "input_schema": {
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"},
            },
            "required": ["path", "old_string", "new_string"],
        },
    },
]


# ── Sandbox tool execution ──────────────────────────────────────────

def safe_path(sandbox, rel):
    full = os.path.realpath(os.path.join(sandbox, rel))
    if not full.startswith(os.path.realpath(sandbox)):
        raise ValueError(f"path escapes sandbox: {rel}")
    return full


def exec_tool(sandbox, name, inp):
    try:
        if name == "bash":
            r = subprocess.run(
                inp["command"], shell=True, cwd=sandbox,
                capture_output=True, text=True, timeout=30,
            )
            out = (r.stdout + r.stderr).strip()
            return out[:8000] if out else "(no output)"
        if name == "read_file":
            with open(safe_path(sandbox, inp["path"])) as f:
                return f.read()[:8000]
        if name == "write_file":
            p = safe_path(sandbox, inp["path"])
            parent = os.path.dirname(p)
            if parent:
                os.makedirs(parent, exist_ok=True)
            with open(p, "w") as f:
                f.write(inp["content"])
            return f"wrote {len(inp['content'])} bytes to {inp['path']}"
        if name == "edit_file":
            p = safe_path(sandbox, inp["path"])
            with open(p) as f:
                content = f.read()
            count = content.count(inp["old_string"])
            if count == 0:
                return "ERROR: old_string not found"
            if count > 1:
                return f"ERROR: old_string matches {count} times, must be unique"
            with open(p, "w") as f:
                f.write(content.replace(inp["old_string"], inp["new_string"]))
            return f"edited {inp['path']}"
        return f"ERROR: unknown tool {name}"
    except Exception as e:
        return f"ERROR: {e}"


# ── Cache strategies ────────────────────────────────────────────────
# Each strategy receives the message list (about to be sent) and mutates
# it to place cache_control markers. Messages are deep-copied first, so
# the canonical history is never contaminated (mirrors prefix-stability).

def _mark_last_block(msg):
    """Put cache_control on the last content block of a message."""
    c = msg.get("content")
    if isinstance(c, str):
        msg["content"] = [{"type": "text", "text": c}]
    if isinstance(msg["content"], list) and msg["content"]:
        msg["content"][-1]["cache_control"] = {"type": "ephemeral"}


def strat_none(msgs):
    return


def strat_single_last(msgs):
    """Claude Code: exactly one marker, on the last message."""
    if msgs:
        _mark_last_block(msgs[-1])


def strat_last_3(msgs):
    """OpenCode: markers on each of the last 3 messages."""
    for m in msgs[-3:]:
        _mark_last_block(m)


def strat_sliding_4(msgs):
    """SynapsCLI current: up to 2 markers on user messages, advancing
    every 4 user messages. Simplified faithful port of
    annotate_cache_breakpoint in runtime/helpers.rs."""
    user_idx = [i for i, m in enumerate(msgs) if m["role"] == "user"]
    if not user_idx:
        return
    # markers persist turn-to-turn in the real impl; here we recompute
    # placement deterministically: mark every 4th user message, keep last 2
    marks = user_idx[3::4]
    if user_idx[-1] not in marks:
        marks.append(user_idx[-1])
    for i in marks[-2:]:
        _mark_last_block(msgs[i])


STRATEGIES = {
    "none": strat_none,
    "single-last": strat_single_last,
    "last-3": strat_last_3,
    "sliding-4": strat_sliding_4,
}


# ── API call ────────────────────────────────────────────────────────

def call_api(api_key, model, messages, strategy_fn, max_retries=4):
    """One non-streaming call. Returns (response_json, elapsed_s)."""
    # Deep-copy so markers never contaminate canonical history
    msgs = json.loads(json.dumps(messages))
    strategy_fn(msgs)

    tools = json.loads(json.dumps(TOOLS))
    tools[-1]["cache_control"] = {"type": "ephemeral"}

    body = {
        "model": model,
        "max_tokens": 4096,
        "system": [
            {
                "type": "text",
                "text": SYSTEM_PROMPT,
                "cache_control": {"type": "ephemeral"},
            }
        ],
        "tools": tools,
        "messages": msgs,
    }

    data = json.dumps(body).encode()
    last_err = None
    for attempt in range(max_retries + 1):
        if attempt:
            delay = 2 ** attempt
            print(f"    retry {attempt}/{max_retries} in {delay}s ({last_err})")
            time.sleep(delay)
        req = urllib.request.Request(
            API_URL,
            data=data,
            headers={
                "x-api-key": api_key,
                "anthropic-version": "2023-06-01",
                "content-type": "application/json",
            },
        )
        t0 = time.monotonic()
        try:
            with urllib.request.urlopen(req, timeout=300) as resp:
                return json.load(resp), time.monotonic() - t0
        except urllib.error.HTTPError as e:
            body_text = e.read().decode(errors="replace")[:300]
            last_err = f"HTTP {e.code}: {body_text}"
            if e.code not in (429, 500, 502, 503, 529):
                raise RuntimeError(last_err)
        except Exception as e:
            last_err = str(e)
    raise RuntimeError(f"API failed after {max_retries} retries: {last_err}")


def extract_usage(resp):
    u = resp.get("usage", {})
    cc = u.get("cache_creation") or {}
    return {
        "input": u.get("input_tokens", 0),
        "output": u.get("output_tokens", 0),
        "cache_read": u.get("cache_read_input_tokens", 0),
        "cache_write": u.get("cache_creation_input_tokens", 0),
        "cache_write_5m": cc.get("ephemeral_5m_input_tokens", 0),
        "cache_write_1h": cc.get("ephemeral_1h_input_tokens", 0),
    }


# Sonnet 4.x pricing per MTok (USD)
PRICE = {"input": 3.0, "output": 15.0, "cache_read": 0.30, "cache_write": 3.75}


def turn_cost(u):
    return (
        u["input"] * PRICE["input"]
        + u["output"] * PRICE["output"]
        + u["cache_read"] * PRICE["cache_read"]
        + u["cache_write"] * PRICE["cache_write"]
    ) / 1_000_000


# ── Agent loop per question ─────────────────────────────────────────

def run_question(api_key, model, messages, q, sandbox, strategy_fn, log):
    """Run one question through the tool loop. Appends to messages in place."""
    messages.append({"role": "user", "content": q["prompt"]})
    turn_usage = []
    api_calls = 0
    final_text = ""

    while True:
        resp, elapsed = call_api(api_key, model, messages, strategy_fn)
        api_calls += 1
        u = extract_usage(resp)
        u["elapsed_s"] = round(elapsed, 2)
        turn_usage.append(u)

        content = resp.get("content", [])
        tool_uses = [b for b in content if b.get("type") == "tool_use"]
        text_parts = [b.get("text", "") for b in content if b.get("type") == "text"]
        final_text = "".join(text_parts)

        messages.append({"role": "assistant", "content": content})

        if not tool_uses:
            break

        results = []
        for tu in tool_uses:
            out = exec_tool(sandbox, tu["name"], tu.get("input", {}))
            results.append({
                "type": "tool_result",
                "tool_use_id": tu["id"],
                "content": out,
            })
        messages.append({"role": "user", "content": results})

    # Aggregate this question's usage
    agg = {k: sum(t[k] for t in turn_usage)
           for k in ("input", "output", "cache_read", "cache_write",
                     "cache_write_5m", "cache_write_1h")}
    total_in = agg["input"] + agg["cache_read"] + agg["cache_write"]
    hit_pct = round(100.0 * agg["cache_read"] / total_in, 1) if total_in else 0.0
    cost = round(sum(turn_cost(t) for t in turn_usage), 6)

    passed = bool(q["verify"](sandbox))
    if passed and "answer_contains" in q:
        passed = q["answer_contains"] in final_text

    rec = {
        "q": q["id"],
        "expects": q["expects"],
        "passed": passed,
        "api_calls": api_calls,
        "usage": agg,
        "hit_pct": hit_pct,
        "cost_usd": cost,
        "elapsed_s": round(sum(t["elapsed_s"] for t in turn_usage), 2),
        "msg_count_after": len(messages),
    }
    log.write(json.dumps(rec) + "\n")
    log.flush()

    status = "PASS" if passed else "FAIL"
    print(f"  Q{q['id']:2d} [{status}] calls={api_calls} "
          f"hit={hit_pct:5.1f}% cost=${cost:.4f} {q['expects'][:46]}")
    return rec


# ── Main ────────────────────────────────────────────────────────────

def main():
    ap = argparse.ArgumentParser(description="Cache strategy benchmark")
    ap.add_argument("--strategy", choices=list(STRATEGIES), default="sliding-4")
    ap.add_argument("--model", default=DEFAULT_MODEL)
    ap.add_argument("--limit", type=int, default=0,
                    help="run only first N questions (0 = all)")
    args = ap.parse_args()

    api_key = os.environ.get("ANTHROPIC_API_KEY")
    if not api_key:
        sys.exit("ANTHROPIC_API_KEY not set")

    qs = QUESTIONS[: args.limit] if args.limit else QUESTIONS
    strategy_fn = STRATEGIES[args.strategy]

    results_dir = os.path.join(os.path.dirname(os.path.abspath(__file__)), "results")
    os.makedirs(results_dir, exist_ok=True)
    stamp = time.strftime("%Y%m%d-%H%M%S")
    log_path = os.path.join(results_dir, f"run-{args.strategy}-{stamp}.jsonl")

    sandbox = tempfile.mkdtemp(prefix="cachebench-")
    print(f"strategy={args.strategy} model={args.model} questions={len(qs)}")
    print(f"sandbox={sandbox}")
    print(f"log={log_path}\n")

    messages = []
    records = []
    t0 = time.monotonic()

    with open(log_path, "w") as log:
        meta = {
            "meta": True, "strategy": args.strategy, "model": args.model,
            "questions": len(qs), "started": stamp, "sandbox": sandbox,
        }
        log.write(json.dumps(meta) + "\n")
        for q in qs:
            try:
                records.append(run_question(
                    api_key, args.model, messages, q, sandbox, strategy_fn, log))
            except RuntimeError as e:
                print(f"  Q{q['id']:2d} [ERROR] {e}")
                log.write(json.dumps({"q": q["id"], "error": str(e)}) + "\n")

        wall = round(time.monotonic() - t0, 1)
        agg = {k: sum(r["usage"][k] for r in records)
               for k in ("input", "output", "cache_read", "cache_write")}
        total_in = agg["input"] + agg["cache_read"] + agg["cache_write"]
        summary = {
            "summary": True,
            "strategy": args.strategy,
            "passed": sum(r["passed"] for r in records),
            "failed": sum(not r["passed"] for r in records),
            "api_calls": sum(r["api_calls"] for r in records),
            "usage": agg,
            "overall_hit_pct": round(100.0 * agg["cache_read"] / total_in, 1)
                               if total_in else 0.0,
            "total_cost_usd": round(sum(r["cost_usd"] for r in records), 4),
            "wall_s": wall,
        }
        log.write(json.dumps(summary) + "\n")

    print(f"\n{'='*60}")
    print(f"strategy={args.strategy}  "
          f"passed={summary['passed']}/{len(records)}  "
          f"api_calls={summary['api_calls']}")
    print(f"hit_rate={summary['overall_hit_pct']}%  "
          f"cost=${summary['total_cost_usd']}  wall={wall}s")
    print(f"log: {log_path}")


if __name__ == "__main__":
    main()
