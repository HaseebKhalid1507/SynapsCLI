"""Offline policy + real subprocess framing tests; all writes stay in temp fixtures."""

from copy import deepcopy
import importlib.util
import io
from itertools import permutations
import json
import os
from pathlib import Path
import selectors
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
import uuid

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("autonomous_example", ROOT / "main.py")
plugin = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = plugin
spec.loader.exec_module(plugin)


def framed(value):
    output = io.BytesIO()
    plugin.write_message(output, value)
    return output.getvalue()


def poll_request(driver, outcome="success", kind="none", decision=None, **updates):
    value = {"run_id": driver.run.run_id, "decision_id": decision or str(uuid.uuid4()),
             "outcome": outcome, "error_kind": kind, **driver.run.selection}
    value.update(updates)
    return value


def send_poll(driver, value):
    return driver.poll([json.dumps(value)])["session_driver"]


class Clock:
    def __init__(self):
        self.now = 1000.0

    def __call__(self):
        return self.now


class PolicyTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.clock = Clock()
        self.driver = plugin.Driver(self.root, self.clock)
        self.driver.initialize({"extension_protocol_version": 1})

    def command(self, *args):
        return self.driver.command(list(args))[0]["session_driver"]

    def start(self, *limits):
        return self.command("start", *limits, "--", "Inspect", "only", "🦉")

    def poll(self, outcome="success", kind="none", **updates):
        return send_poll(self.driver, poll_request(self.driver, outcome, kind, **updates))

    def test_manifest_minimal_eager_command(self):
        value = json.loads((ROOT / ".synaps-plugin/plugin.json").read_text())
        self.assertEqual(value["version"], "0.1.5")
        extension = value["extension"]
        self.assertEqual(extension["command"], "python3")
        self.assertEqual(extension["args"], ["main.py"])
        self.assertEqual(extension["permissions"], ["session.drive"])
        self.assertEqual(extension["hooks"], [])
        self.assertNotIn("deferred", extension)
        self.assertNotIn("activation", extension)
        self.assertEqual(value["commands"][0]["name"], "auto")
        self.assertIs(value["commands"][0]["interactive"], True)
        self.assertEqual(len(value["commands"]), 1)

    def test_init_status_and_favorites_never_start_or_write(self):
        self.assertEqual(self.command("status")["action"], "status")
        result, rows = self.driver.command(["favorites"])
        self.assertEqual(result["session_driver"]["action"], "status")
        self.assertEqual(rows, [
            ["1", "openai-codex/gpt-6-astra", "ultra"],
            ["2", "anthropic/claude-fable-5-1", "xhigh"],
            ["3", "kimi-code/k3", "max"],
            ["4", "x-ai/grok-4.6", "high"],
        ])
        self.assertIsNone(self.driver.run)
        self.assertEqual(list(self.root.iterdir()), [])
        self.assertEqual(self.driver.poll([])["session_driver"]["action"], "stop")

    def test_time_checkpoint_keeps_favorite_and_success_limit_and_replay_safety(self):
        start = self.start("--turns", "1", "--context", "off")
        self.assertEqual(start["time_checkpoint_version"], 1)
        selection = deepcopy(self.driver.run.selection)
        request = poll_request(self.driver, "time_checkpoint", "wall_clock")
        result = send_poll(self.driver, request)
        self.assertEqual(result["action"], "next")
        self.assertEqual(result["selection"], selection)
        self.assertIn("not a provider failure", result["prompt"])
        self.assertIn("Never replay side effects", result["prompt"])
        self.assertNotIn("materially different approach", result["prompt"])
        self.assertEqual(self.driver.run.successes, 0)
        self.assertEqual(self.driver.run.repeat_streak, 0)
        self.assertEqual(send_poll(self.driver, request), result)
        self.assertEqual(send_poll(self.driver, poll_request(self.driver))["action"], "stop")

    def test_time_checkpoint_deadline_and_inconsistent_metadata_stop(self):
        for fields in [dict(error_kind="auth"), dict(feedback="changed")]:
            self.start()
            request = poll_request(self.driver, "time_checkpoint", "wall_clock")
            request.update(fields)
            self.assertEqual(send_poll(self.driver, request)["action"], "stop")
        self.start("--for", "1m")
        self.clock.now += 60
        self.assertEqual(send_poll(self.driver, poll_request(self.driver, "time_checkpoint", "wall_clock"))["action"], "stop")

    def test_start_fields_exact_defaults_and_safety_prompt(self):
        start = self.start()
        self.assertEqual(set(start), {"action", "run_id", "models", "prompt", "delay_ms", "notice",
                                      "feedback_version", "time_checkpoint_version", "context_mode"})
        self.assertIs(type(start["feedback_version"]), int)
        self.assertEqual(start["feedback_version"], 1)
        self.assertEqual(start["context_mode"], "auto")
        self.assertEqual(str(uuid.UUID(start["run_id"])), start["run_id"])
        self.assertEqual(start["models"], plugin.defaults())
        self.assertEqual(start["models"][0], self.driver.run.selection)
        self.assertEqual(start["delay_ms"], 1000)
        self.assertIn("charges", start["notice"])
        self.assertIn("across", start["notice"])
        self.assertIsNone(self.driver.run.turns)
        self.assertIsNone(self.driver.run.deadline)
        result = self.poll()
        self.assertEqual(set(result), {"action", "run_id", "selection", "prompt", "delay_ms", "notice"})
        self.assertEqual(result["selection"], start["models"][0])
        for phrase in ("autonomous loop", "Inspect only 🦉", "Do not repeat completed external actions",
                       "Do not expand", "permissions", "human-needed", "stop"):
            self.assertIn(phrase, result["prompt"])
        self.assertEqual(self.driver.run.successes, 1)
        self.assertLessEqual(len(result["prompt"].encode()), 16384)
        self.assertLessEqual(len(plugin.json_bytes({"session_driver": result})), plugin.MAX_FRAME)

    def test_context_start_modes_disclosure_and_no_plugin_persistence(self):
        for flags, mode in (((), "auto"), (("--context", "auto"), "auto"),
                            (("--context", "off"), "off")):
            with self.subTest(mode=mode, flags=flags):
                start = self.start(*flags)
                self.assertEqual(start["context_mode"], mode)
                self.assertEqual(set(start), {"action", "run_id", "models", "prompt", "delay_ms",
                                              "notice", "feedback_version", "time_checkpoint_version", "context_mode"})
                for phrase in (
                    f"Context mode: {mode} (foreground session only)",
                    "applied only after host grant acceptance",
                    "the same validation as /context auto/off",
                    "system/developer prompts, private reasoning, restricted/sensitive content and binary blocks",
                    "redaction is not a universal secret detector",
                    "persists in the current session after the run stops",
                    "No global config or memory recall/capture consent changes",
                    "delegated worker context is unchanged",
                    "charges", "across favorite providers", "normal permissions still apply",
                    "Stop with Esc or /auto stop",
                ):
                    self.assertIn(phrase, start["notice"])
                if mode == "auto":
                    self.assertIn("local archival of eligible session content", start["notice"])
                else:
                    self.assertIn("Off disables automatic context continuation", start["notice"])
                    self.assertIn("existing archives are not deleted", start["notice"])
                self.assertLessEqual(len(start["notice"].encode("utf-8")), 2 * 1024)
                self.assert_latest_human_steering_prompt(start, "Inspect only 🦉")
                self.assertEqual(set(self.command("status")), {"action", "notice"})
                self.assertEqual(set(self.command("stop")), {"action", "notice"})
                self.assertEqual(list(self.root.iterdir()), [])
        # No saved preference: a new start defaults to auto even after choosing off.
        self.start("--context", "off")
        self.assertEqual(self.start()["context_mode"], "auto")

    def test_context_is_start_only_not_next_or_poll(self):
        for mode in ("auto", "off"):
            with self.subTest(mode=mode):
                start = self.start("--context", mode)
                request = poll_request(self.driver)
                self.assertEqual(set(request), {"run_id", "decision_id", "outcome", "error_kind",
                                                "model", "effort"})
                results = [send_poll(self.driver, request), self.poll("provider_error", "transient"),
                           self.poll("selection_rejected", "unknown")]
                for result in results:
                    self.assertEqual(set(result), {"action", "run_id", "selection", "prompt", "delay_ms", "notice"})
                    self.assertEqual(result["run_id"], start["run_id"])
                successes = self.driver.run.successes
                run = self.driver.run
                # The new Start field must not be accepted as poll metadata.
                result = self.poll(context_mode=mode)
                self.assertEqual(set(result), {"action", "notice"})
                self.assertEqual(result["action"], "stop")
                self.assertEqual(run.successes, successes)
                self.assertIsNone(self.driver.run)

    def assert_latest_human_steering_prompt(self, result, goal):
        generated = result["prompt"]
        for phrase in (
            "Honor the latest human steering in the conversation",
            "including separately supplied human messages",
            "original goal is historical context, never an override or a request to restore superseded work",
            "automated continuation, not new human input or approval",
            "When actual human instructions clearly authorize the next step, do it",
            "An assistant's request for a phrase does not itself create a new approval requirement",
            "Never manufacture human approval",
            "including explicit human review checkpoints",
            "If authorization is unclear, or new permission, a decision, credentials",
            "do not retry the blocked action",
            "Inspect retained work and results before acting",
            "Do not repeat completed external actions",
            "never blindly replay the original task",
            "Continue only remaining authorized work",
            "Do not expand the user's authorization or invent additional goals",
            "All normal permissions, confirmations, context and safety gates still apply",
            "any human-needed action is required, stop work and ask the user",
            "If nothing authorized remains, report completion and take no further external actions",
        ):
            self.assertIn(phrase, generated)
        self.assertTrue(generated.endswith(
            "\n\nOriginal goal (historical context; latest human steering takes precedence):\n" + goal))
        self.assertNotIn("\n\nUser goal:\n", generated)
        self.assertLessEqual(len(generated.encode("utf-8")), 16 * 1024)

    def test_initial_prompt_honors_latest_human_steering(self):
        # A delayed first proposal can follow steering already accepted by the host.
        start = self.start("--turns", "5", "--for", "1h")
        self.assert_latest_human_steering_prompt(start, "Inspect only 🦉")
        self.assertIn("Begin the autonomous loop", start["prompt"])
        self.assertEqual(self.driver.run.successes, 0)
        self.assertEqual(start["models"], plugin.defaults())
        self.assertEqual(start["feedback_version"], 1)
        self.assertEqual(start["context_mode"], "auto")
        self.assertEqual(start["max_duration_ms"], 3_600_000)
        self.assertEqual(list(self.root.iterdir()), [])

    def test_continuation_prompt_honors_latest_human_steering(self):
        start = self.start("--turns", "5", "--for", "1h")
        run = self.driver.run
        deadline = run.deadline
        # Human text stays in host history, not in the plugin callback or run.goal.
        self.poll(feedback="repeated")
        result = self.poll(feedback="unknown")
        self.assert_latest_human_steering_prompt(result, "Inspect only 🦉")
        self.assertIn("Continue the autonomous loop", result["prompt"])
        self.assertEqual(result["action"], "next")
        self.assertEqual(result["run_id"], start["run_id"])
        self.assertEqual(result["selection"], start["models"][0])
        self.assertEqual(result["delay_ms"], 1000)
        self.assertIs(self.driver.run, run)
        self.assertEqual(run.goal, "Inspect only 🦉")
        self.assertEqual(run.models, start["models"])
        self.assertEqual((run.turns, run.successes, run.repeat_streak, run.retries), (5, 2, 0, 0))
        self.assertEqual(run.deadline, deadline)
        self.assertEqual(list(self.root.iterdir()), [])

    def test_recovery_prompts_honor_latest_human_steering(self):
        cases = [("provider_error", kind, 1, 0, 2000, "previous provider attempt failed")
                 for kind in ("transient", "rate_limit")]
        cases += [("provider_error", kind, 1, 1, 1000, "previous provider attempt failed")
                  for kind in ("auth", "quota")]
        cases += [
            ("selection_rejected", "unknown", 1, 1, 1000, "previous exact model/effort selection was rejected"),
            ("selection_rejected", "unknown", 4, 0, 300000, "previous exact model/effort selection was rejected"),
            ("success", "none", 3, 1, 1000, "feedback suggests repetition/stalling or empty output"),
            ("success", "none", 12, 0, 300000, "feedback suggests repetition/stalling or empty output"),
        ]
        for outcome, kind, count, index, delay, reason in cases:
            with self.subTest(outcome=outcome, kind=kind, count=count):
                start = self.start("--turns", "20", "--for", "1h")
                run = self.driver.run
                deadline = run.deadline
                for _ in range(count):
                    result = self.poll(outcome, kind, feedback=(
                        "repeated" if outcome == "success" else "unknown"))
                self.assert_latest_human_steering_prompt(result, "Inspect only 🦉")
                self.assertIn("Continue the autonomous loop", result["prompt"])
                self.assertIn(reason, result["prompt"])
                self.assertIn("materially different approach", result["prompt"])
                self.assertIn("Never replay side effects or bypass gates", result["prompt"])
                self.assertEqual(result["action"], "next")
                self.assertEqual(result["run_id"], start["run_id"])
                self.assertEqual(result["selection"], start["models"][index])
                self.assertEqual(result["delay_ms"], delay)
                self.assertIs(self.driver.run, run)
                self.assertEqual(run.goal, "Inspect only 🦉")
                self.assertEqual(run.models, start["models"])
                self.assertEqual(run.successes, count if outcome == "success" else 0)
                self.assertEqual(run.turns, 20)
                self.assertEqual(run.deadline, deadline)
                self.assertEqual(list(self.root.iterdir()), [])

    def test_initial_and_failed_attempt_count(self):
        self.start("--turns", "1")
        for outcome, kind in [("selection_rejected", "unknown"), ("provider_error", "auth")]:
            result = self.poll(outcome, kind)
            self.assertEqual(result["action"], "next")
            self.assertIn("Inspect only 🦉", result["prompt"])
            self.assertEqual(self.driver.run.successes, 0)
        result = self.poll()
        self.assertEqual(result["action"], "stop")
        self.assertIn("1/1", result["notice"])
        self.assertIsNone(self.driver.run)

    def test_two_successes_including_initial(self):
        self.start("--turns", "2")
        self.assertEqual(self.poll()["action"], "next")
        self.assertEqual(self.driver.run.successes, 1)
        self.assertEqual(self.poll()["action"], "stop")

    def test_completed_turn_feedback_threshold_and_empty_repeated_mix(self):
        for labels in (("repeated",) * 3, ("empty",) * 3,
                       ("repeated", "empty", "repeated"), ("empty", "repeated", "empty")):
            with self.subTest(labels=labels):
                start = self.start()
                for count, label in enumerate(labels, 1):
                    result = self.poll(feedback=label)
                    self.assertEqual(result["action"], "next")
                    self.assertEqual(self.driver.run.successes, count)
                    self.assertEqual(result["selection"], start["models"][0 if count < 3 else 1])
                    self.assertEqual(result["delay_ms"], 1000)
                    self.assertEqual(self.driver.run.repeat_streak, count if count < 3 else 0)
                self.assertEqual(self.driver.run.retries, 0)
                for selection in start["models"][:2]:
                    self.assertIn(f"{selection['model']} {selection['effort']}", result["notice"])
                self.assertIn("3 times", result["notice"])
                for phrase in ("feedback suggests repetition/stalling or empty output", "Inspect retained results",
                               "materially different approach", "Never replay side effects or bypass gates",
                               "Inspect only 🦉", "human-needed", "Do not repeat completed external actions"):
                    self.assertIn(phrase, result["prompt"])
                # A new exact selection gets its own fresh streak, not its predecessor's.
                self.assertEqual(self.poll(feedback="repeated")["selection"], start["models"][1])
                self.assertEqual(self.driver.run.repeat_streak, 1)
                recovered = self.poll(feedback="changed")
                self.assertEqual(recovered["prompt"], plugin.prompt(self.driver.run.goal, True))
                self.assertEqual(self.driver.run.repeat_streak, 0)

    def test_first_feedback_correction_preserves_grant_accounting_and_duplicate(self):
        for first, second in (("repeated", "empty"), ("empty", "repeated")):
            with self.subTest(first=first):
                start = self.start("--turns", "8", "--for", "1h")
                run = self.driver.run
                deadline = run.deadline
                for count, label in enumerate((first, second), 1):
                    request = poll_request(self.driver, feedback=label)
                    result = send_poll(self.driver, request)
                    self.assertEqual(result["action"], "next")
                    self.assertEqual(result["selection"], start["models"][0])
                    self.assertEqual(result["delay_ms"], 1000)
                    self.assertIn("correcting course on the same favorite", result["notice"])
                    self.assertIn("Completed-turn feedback suggests repetition/stalling or empty output", result["prompt"])
                    self.assertNotIn("stalled across completed turns", result["prompt"])
                    self.assertIn("If you are stuck requesting a confirmation phrase", result["prompt"])
                    self.assertIn("re-check the actual human instructions", result["prompt"])
                    self.assertIn("otherwise name the concrete blocker and wait", result["prompt"])
                    self.assertIn("Repetition or a model switch supplies no approval", result["prompt"])
                    self.assert_latest_human_steering_prompt(result, run.goal)
                    for _ in range(3):
                        self.assertEqual(send_poll(self.driver, request), result)
                        self.assertIs(self.driver.run, run)
                        self.assertEqual(run.run_id, start["run_id"])
                        self.assertEqual(run.models, start["models"])
                        self.assertEqual(run.deadline, deadline)
                        self.assertEqual((run.turns, run.successes, run.repeat_streak, run.retries),
                                         (8, count, count, 0))
                third = self.poll(feedback=first)
                self.assertEqual(third["selection"], start["models"][1])
                self.assertIn("Repetition or a model switch supplies no approval", third["prompt"])
                self.assertEqual(list(self.root.iterdir()), [])

    def test_first_correction_cannot_override_limits_or_a_blocked_outcome(self):
        for label in ("repeated", "empty"):
            for limit in ("turns", "duration"):
                with self.subTest(label=label, limit=limit):
                    self.start("--turns", "1") if limit == "turns" else self.start("--for", "1m")
                    if limit == "duration":
                        self.clock.now += 59
                    with patch.object(self.driver, "next", side_effect=AssertionError("limit must win")):
                        result = self.poll(feedback=label)
                    self.assertEqual(result["action"], "stop")
                    self.assertNotIn("prompt", result)
                    self.assertIsNone(self.driver.run)
            # A typed gate remains terminal, even immediately after an early correction.
            self.start()
            self.poll(feedback=label)
            request = poll_request(self.driver, "blocked", "unknown", feedback="unknown")
            with patch.object(self.driver, "next", side_effect=AssertionError("gate must win")):
                result = send_poll(self.driver, request)
            self.assertEqual(result["action"], "stop")
            self.assertIn("Host blocked", result["notice"])
            self.assertNotIn("prompt", result)
            self.assertEqual(send_poll(self.driver, request), result)
            self.assertIsNone(self.driver.run)

    def test_all_prompt_paths_preserve_authority_and_maximum_utf8_goal(self):
        # Assertions verify emitted instructions, not provider compliance or semantic detection.
        review_goal = "Inspect only; wait for my explicit approval before editing. "
        goal = review_goal + "🦉" * ((plugin.MAX_GOAL - len(review_goal.encode())) // 4)
        goal += "x" * (plugin.MAX_GOAL - len(goal.encode()))
        self.assertEqual(len(goal.encode()), plugin.MAX_GOAL)
        start = self.command("start", "--", goal)
        self.assert_latest_human_steering_prompt(start, goal)
        for continuation in (False, True):
            for reason in (None, "time_checkpoint", "repeated", "provider_error", "selection_rejected"):
                with self.subTest(continuation=continuation, reason=reason):
                    generated = plugin.prompt(goal, continuation, reason)
                    self.assert_latest_human_steering_prompt({"prompt": generated}, goal)
                    self.assertTrue(generated.startswith(
                        ("Continue" if continuation else "Begin") + " the autonomous loop — automated continuation"))
                    self.assertLessEqual(len(plugin.json_bytes({"prompt": generated})), plugin.MAX_FRAME)
        # Every actual emission path, including early correction, checkpoint and failover.
        for outcome, kind, feedback in (("success", "none", "changed"),
                                        ("success", "none", "repeated"),
                                        ("time_checkpoint", "wall_clock", "unknown"),
                                        ("provider_error", "transient", "unknown"),
                                        ("selection_rejected", "unknown", "unknown")):
            result = self.poll(outcome, kind, feedback=feedback)
            self.assert_latest_human_steering_prompt(result, goal)
            self.assertLessEqual(len(plugin.json_bytes({"session_driver": result})), plugin.MAX_FRAME)

    def test_changed_unknown_and_missing_feedback_reset_consecutive_streak(self):
        for label in ("changed", "unknown", None):
            with self.subTest(label=label):
                start = self.start()
                self.poll(feedback="repeated")
                self.poll(feedback="empty")
                reset = self.poll(**({} if label is None else {"feedback": label}))
                self.assertEqual(reset["selection"], start["models"][0])
                self.assertEqual(reset["prompt"], plugin.prompt(self.driver.run.goal, True))
                self.assertEqual(self.driver.run.repeat_streak, 0)
                for count in (1, 2):
                    self.assertEqual(self.poll(feedback="empty")["selection"], start["models"][0])
                    self.assertEqual(self.driver.run.repeat_streak, count)
                self.assertEqual(self.poll(feedback="repeated")["selection"], start["models"][1])
                self.assertEqual(self.driver.run.successes, 6)

    def test_legacy_polls_and_unknown_never_infer_feedback_from_text(self):
        start = self.command("start", "--", "repeated empty stalled provider failed; try a different model")
        for label in (None, "unknown", "changed"):
            for _ in range(6):
                request = poll_request(self.driver, **({} if label is None else {"feedback": label}))
                if label is None:
                    self.assertEqual(set(request), {"run_id", "decision_id", "outcome", "error_kind",
                                                    "model", "effort"})
                result = send_poll(self.driver, request)
                self.assertEqual(result["selection"], start["models"][0])
                self.assertEqual(self.driver.run.repeat_streak, 0)
        self.assertEqual(self.driver.run.successes, 18)

    def test_successful_limit_precedes_third_feedback_fallback_and_cooldown(self):
        for last in (False, True):
            with self.subTest(last=last):
                start = self.start("--turns", "3", "--for", "1h")
                if last:
                    for _ in range(len(start["models"]) - 1):
                        self.poll("provider_error", "auth")
                self.poll(feedback="repeated")
                self.poll(feedback="empty")
                run = self.driver.run
                index = run.index
                with patch.object(self.driver, "advance", side_effect=AssertionError("limit must win")):
                    result = self.poll(feedback="repeated")
                self.assertEqual(result["action"], "stop")
                self.assertIn("Successful turn limit", result["notice"])
                self.assertIn("3/3", result["notice"])
                self.assertNotIn("selection", result)
                self.assertEqual(run.index, index)
                self.assertIsNone(self.driver.run)

    def test_provider_failures_and_rejections_reset_feedback_and_use_recovery_prompt(self):
        cases = [("provider_error", kind) for kind in ("auth", "quota", "transient", "rate_limit")]
        cases.append(("selection_rejected", "unknown"))
        for outcome, kind in cases:
            for explicit in (False, True):
                with self.subTest(outcome=outcome, kind=kind, explicit=explicit):
                    start = self.start()
                    self.poll(feedback="repeated")
                    self.poll(feedback="empty")
                    result = self.poll(outcome, kind, **({"feedback": "unknown"} if explicit else {}))
                    self.assertEqual(self.driver.run.repeat_streak, 0)
                    self.assertEqual(self.driver.run.successes, 2)
                    retrying = kind in ("transient", "rate_limit")
                    self.assertEqual(self.driver.run.retries, 1 if retrying else 0)
                    self.assertEqual(result["selection"], start["models"][0 if retrying else 1])
                    self.assertEqual(result["delay_ms"], 2000 if retrying else 1000)
                    expected = ("previous provider attempt failed" if outcome == "provider_error"
                                else "previous exact model/effort selection was rejected")
                    self.assertIn(expected, result["prompt"])
                    for phrase in ("Inspect retained results", "materially different approach",
                                   "Never replay side effects or bypass gates", "human-needed"):
                        self.assertIn(phrase, result["prompt"])
                    selection = result["selection"]
                    for count in (1, 2):
                        self.assertEqual(self.poll(feedback="repeated")["selection"], selection)
                        self.assertEqual(self.driver.run.repeat_streak, count)
                        self.assertEqual(self.driver.run.retries, 0)
                    switched = self.poll(feedback="empty")
                    self.assertNotEqual(switched["selection"], selection)
                    self.assertEqual(self.driver.run.repeat_streak, 0)
                    self.assertEqual(self.driver.run.retries, 0)
                    self.assertEqual(self.poll("provider_error", "transient")["delay_ms"], 2000)

    def test_duplicate_feedback_does_not_double_count_even_at_switch(self):
        start = self.start()
        for count, label in enumerate(("repeated", "empty", "repeated"), 1):
            request = poll_request(self.driver, feedback=label)
            result = send_poll(self.driver, request)
            for _ in range(5):
                self.assertEqual(send_poll(self.driver, request), result)
                self.assertEqual(self.driver.run.successes, count)
                self.assertEqual(self.driver.run.repeat_streak, count if count < 3 else 0)
                self.assertEqual(result["selection"], start["models"][0 if count < 3 else 1])
        self.assertEqual(self.poll(feedback="empty")["selection"], start["models"][1])
        self.assertEqual(self.driver.run.repeat_streak, 1)

    def test_optional_feedback_presence_and_value_are_in_duplicate_fingerprint(self):
        for before, after in ((None, "unknown"), ("unknown", None), ("repeated", "empty"),
                              ("empty", "changed"), ("changed", "unknown")):
            with self.subTest(before=before, after=after):
                self.start()
                request = poll_request(self.driver, **({} if before is None else {"feedback": before}))
                send_poll(self.driver, request)
                run = self.driver.run
                if after is None:
                    request.pop("feedback")
                else:
                    request["feedback"] = after
                result = send_poll(self.driver, request)
                self.assertEqual(result["action"], "stop")
                self.assertIn("changed payload", result["notice"])
                self.assertEqual(run.successes, 1)

    def test_invalid_optional_feedback_fails_closed_without_echo_or_accounting(self):
        bad = (None, True, False, 1, 1.5, [], {}, "", "Repeated", "repeated ", "overflow",
               "stalled", "repeated\n", "raw secret error text", "x" * 4096)
        for label in bad:
            with self.subTest(label=str(label)[:80]):
                self.start()
                run = self.driver.run
                result = self.poll(feedback=label)
                self.assertEqual(result["action"], "stop")
                self.assertEqual(result["notice"],
                                 "Malformed or mismatched driver callback; stopped safely. 0/unbounded successful turns.")
                self.assertEqual(run.successes, 0)
                self.assertEqual(run.repeat_streak, 0)
                self.assertIsNone(self.driver.run)
        for outcome, kind in (("provider_error", "auth"), ("selection_rejected", "unknown"),
                              ("blocked", "unknown")):
            for label in ("changed", "repeated", "empty"):
                self.start()
                run = self.driver.run
                self.assertEqual(self.poll(outcome, kind, feedback=label)["action"], "stop")
                self.assertEqual(run.successes, 0)
                self.assertEqual(run.index, 0)
                self.assertEqual(run.retries, 0)
        # An allowed unknown feedback label still never retries blocked/unknown provider failures.
        for outcome in ("blocked", "provider_error"):
            self.start()
            self.poll(feedback="repeated")
            run = self.driver.run
            self.assertEqual(self.poll(outcome, "unknown", feedback="unknown")["action"], "stop")
            self.assertEqual(run.repeat_streak, 0)
            self.assertEqual(run.successes, 1)
            self.assertEqual(run.retries, 0)
        # The six original fields remain required, even when feedback is supplied.
        for key in ("run_id", "decision_id", "outcome", "error_kind", "model", "effort"):
            self.start()
            request = poll_request(self.driver, feedback="repeated")
            del request[key]
            self.assertEqual(send_poll(self.driver, request)["action"], "stop")
        self.start()
        raw = json.dumps(poll_request(self.driver, feedback="repeated"))
        self.assertEqual(self.driver.poll([raw[:-1] + ',"feedback":"empty"}'])["session_driver"]["action"], "stop")
        for extra in ({"feedback_version": 1}, {"transcript": "repeated"}, {"hash": "repeated"}):
            self.start()
            self.assertEqual(self.poll(feedback="unknown", **extra)["action"], "stop")

    def test_feedback_all_favorites_cooldown_wrap_and_fresh_streak(self):
        for single in (False, True):
            with self.subTest(single=single):
                if single:
                    self.command("favorites", "set", "custom/model", "ultracode")
                start = self.start("--for", "1h")
                count = len(start["models"])
                for index in range(count):
                    for label in ("repeated", "empty", "repeated"):
                        request = poll_request(self.driver, feedback=label)
                        result = send_poll(self.driver, request)
                    self.assertEqual(result["selection"], start["models"][(index + 1) % count])
                    self.assertEqual(result["delay_ms"], 300000 if index == count - 1 else 1000)
                    self.assertEqual(self.driver.run.successes, 3 * (index + 1))
                    self.assertEqual(self.driver.run.repeat_streak, 0)
                    self.assertEqual(self.driver.run.retries, 0)
                    for selected in (start["models"][index], result["selection"]):
                        self.assertIn(f"{selected['model']} {selected['effort']}", result["notice"])
                self.assertIn("cooldown 5 minutes", result["notice"])
                self.assertIn("feedback suggests repetition/stalling or empty output", result["prompt"])
                self.assertEqual(send_poll(self.driver, request), result)
                self.clock.now += 300
                self.assertEqual(self.poll(feedback="repeated")["selection"], start["models"][0])
                self.assertEqual(self.driver.run.repeat_streak, 1)

    def test_feedback_recovery_respects_deadline_and_duplicate_cooldown_deadline(self):
        for elapsed in (59, 60):
            self.start("--for", "1m")
            self.poll(feedback="repeated")
            self.poll(feedback="empty")
            run = self.driver.run
            self.clock.now += elapsed
            with patch.object(self.driver, "advance", side_effect=AssertionError("deadline must win")):
                result = self.poll(feedback="repeated")
            self.assertEqual(result["action"], "stop")
            self.assertIn("Duration limit", result["notice"])
            self.assertEqual(run.index, 0)
            self.assertEqual(run.successes, 3 if elapsed == 59 else 2)
        self.command("favorites", "set", "custom/model", "ultra")
        self.start("--for", "5m")
        self.poll(feedback="empty")
        self.poll(feedback="repeated")
        result = self.poll(feedback="empty")
        self.assertEqual(result["action"], "stop")
        self.assertIn("during delay", result["notice"])
        self.assertIn("3/unbounded", result["notice"])
        self.start("--for", "6m")
        self.poll(feedback="empty")
        self.poll(feedback="empty")
        request = poll_request(self.driver, feedback="empty")
        self.assertEqual(send_poll(self.driver, request)["delay_ms"], 300000)
        self.clock.now += 60
        self.assertEqual(send_poll(self.driver, request)["action"], "stop")

    def test_feedback_restart_stop_and_reinitialize_forget_streak_and_cache(self):
        for operation in ("stop", "initialize", "restart", "start"):
            with self.subTest(operation=operation):
                self.start()
                self.poll(feedback="repeated")
                self.poll(feedback="empty")
                stale = poll_request(self.driver, feedback="repeated")
                if operation == "stop":
                    self.command("stop")
                elif operation == "initialize":
                    self.driver.initialize({})
                elif operation == "restart":
                    self.driver = plugin.Driver(self.root, self.clock)
                    self.driver.initialize({})
                else:
                    self.start()
                self.assertEqual(send_poll(self.driver, stale)["action"], "stop")
                start = self.start()
                self.assertEqual(self.driver.run.repeat_streak, 0)
                self.assertEqual(self.driver.run.retries, 0)
                self.assertEqual(self.driver.run.successes, 0)
                self.assertEqual(len(self.driver.cache), 0)
                self.assertEqual(self.poll(feedback="repeated")["selection"], start["models"][0])
                self.assertEqual(self.driver.run.repeat_streak, 1)
                self.assertEqual(list(self.root.iterdir()), [])

    def test_limits_either_order_maxima_and_no_prose_guessing(self):
        parsed = plugin.parse_start(["--for", "365d", "--turns", "1000000", "--", "goal"])
        self.assertEqual(parsed, ("goal", 1000000, plugin.MAX_DURATION_MS, "auto"))
        for value, expected in [("30m", 1_800_000), ("2h", 7_200_000), ("1d", 86_400_000)]:
            with self.subTest(value=value):
                start = self.start("--turns", "5", "--for", value)
                self.assertEqual(start["max_duration_ms"], expected)
                self.assertEqual(self.driver.run.deadline, self.clock() + expected / 1000)
        self.command("start", "--", "work for 30 minutes --turns 2")
        self.assertIsNone(self.driver.run.deadline)
        self.assertIsNone(self.driver.run.turns)

    def test_context_parser_defaults_mixed_flag_order_and_literal_goal_separator(self):
        self.assertEqual(plugin.parse_start(["--", "new", "prompt", "goal"]),
                         ("new prompt goal", None, None, "auto"))
        for mode in ("auto", "off"):
            self.assertEqual(plugin.parse_start(["--context", mode, "--", "new prompt goal"]),
                             ("new prompt goal", None, None, mode))
            for order in permutations((("--context", mode), ("--turns", "5"), ("--for", "2h"))):
                with self.subTest(mode=mode, order=order):
                    flags = [part for pair in order for part in pair]
                    # Flag-looking goal text is literal, not parsed or used as consent.
                    words = ["new", "prompt", "goal", "🦉", "--context", "invalid", "--turns", "0", "--"]
                    goal = " ".join(words)
                    self.assertEqual(plugin.parse_start(flags + ["--"] + words),
                                     (goal, 5, 7_200_000, mode))
                    start = self.command("start", *flags, "--", *words)
                    self.assertEqual(start["context_mode"], mode)
                    self.assertEqual(start["max_duration_ms"], 7_200_000)
                    self.assertEqual(self.driver.run.turns, 5)
                    self.assert_latest_human_steering_prompt(start, goal)
        goal = "new prompt goal --context off --for 1h"
        self.assertEqual(plugin.parse_start(["--", goal]), (goal, None, None, "auto"))
        start = self.command("start", "--", goal)
        self.assertEqual(start["context_mode"], "auto")
        self.assert_latest_human_steering_prompt(start, goal)

    def test_context_parser_rejects_invalid_duplicate_missing_without_mutation(self):
        invalid = [
            ["--context"], ["--context", "--", "goal"],
            ["--context", "--turns", "2", "--", "goal"],
            ["--context", "--for", "1h", "--", "goal"],
            ["--context", "auto"], ["--context", "off", "goal"],
            ["--context", "auto", "--"], ["--context", "off", "--", " \t"],
            ["--context=off", "--", "goal"], ["--Context", "auto", "--", "goal"],
        ]
        invalid += [["--context", value, "--", "goal"] for value in
                    ("", "on", "AUTO", "Off", "none", "manual", "null", "0", "1", " auto", "off ", "auto|off")]
        invalid += [["--context", first, "--turns", "2", "--context", second, "--", "goal"]
                    for first in ("auto", "off") for second in ("auto", "off")]
        self.start("--context", "off")
        self.poll(feedback="repeated")
        run = self.driver.run
        snapshot, cache = deepcopy(run), deepcopy(self.driver.cache)
        for args in invalid:
            with self.subTest(args=args):
                with self.assertRaises(plugin.Invalid):
                    plugin.parse_start(args)
                with self.assertRaises(plugin.Invalid):
                    self.driver.command(["start"] + args)
                self.assertIs(self.driver.run, run)
                self.assertEqual(run, snapshot)
                self.assertEqual(self.driver.cache, cache)
                self.assertEqual(list(self.root.iterdir()), [])

    def test_invalid_limits_goals_and_commands_leave_run_unchanged(self):
        self.start()
        run = self.driver.run
        invalid = [[], ["start"], ["start", "goal"], ["start", "--"], ["start", "--", " \t"],
                   ["start", "--", "x" * (plugin.MAX_GOAL + 1)],
                   ["start", "--", "🦉" * (plugin.MAX_GOAL // 4 + 1)],
                   ["start", "--", "\x1b[31m"], ["start", "--", "\ud800"],
                   ["status", "now"], ["stop", "all"], ["START", "--", "x"], ["favorites", "set"],
                   ["start", "--turns", "1", "--turns", "2", "--", "g"],
                   ["start", "--for", "1h", "--for", "2h", "--", "g"],
                   ["start", "--turns=1", "--", "g"], ["start", "--unknown", "1", "--", "g"],
                   ["start", "--turns"], [True], "start -- goal", ["x"] * 4097]
        for flag, values in [("--turns", ["0", "-1", "+1", "01", "1.0", "1e3", "１", "1000001", "9" * 200]),
                             ("--for", ["0m", "-1h", "01h", "1.5h", "1s", "1w", "1H", "30 minutes", "366d", "9" * 100 + "m"])]:
            invalid.extend(["start", flag, value, "--", "goal"] for value in values)
        for args in invalid:
            with self.subTest(args=str(args)[:120]):
                with self.assertRaises(plugin.Invalid):
                    self.driver.command(args)
                self.assertIs(self.driver.run, run)

    def test_monotonic_deadline_and_delay_boundary(self):
        self.start("--for", "1m")
        self.clock.now += 59
        result = self.poll()
        self.assertEqual(result["action"], "stop")
        self.assertIn("during delay", result["notice"])
        self.start("--for", "1m")
        self.clock.now += 60
        with patch.object(plugin.time, "time", return_value=-1000000):
            self.assertEqual(self.poll()["action"], "stop")
        self.start("--for", "1m")
        self.clock.now += 61
        self.assertEqual(self.command("status")["action"], "status")
        self.assertIsNone(self.driver.run)

    def test_retries_exponential_finite_and_reset_on_success(self):
        for kind in ("rate_limit", "transient"):
            with self.subTest(kind=kind):
                start = self.start()
                for retry, delay in enumerate((2000, 4000, 8000), 1):
                    result = self.poll("provider_error", kind)
                    self.assertEqual(result["delay_ms"], delay)
                    self.assertEqual(result["selection"], start["models"][0])
                    self.assertIn(f"retry {retry}/3", result["notice"])
                    self.assertEqual(self.driver.run.successes, 0)
                result = self.poll("provider_error", kind)
                self.assertEqual(result["selection"], start["models"][1])
                self.assertEqual(result["delay_ms"], 1000)
                self.poll("provider_error", kind)
                self.poll()
                self.assertEqual(self.driver.run.retries, 0)
                self.assertEqual(self.poll("provider_error", kind)["delay_ms"], 2000)

    def test_account_errors_unsupported_exact_selection_and_all_down_cooldown(self):
        for outcome, kind in [("provider_error", "auth"), ("provider_error", "quota"),
                              ("selection_rejected", "unknown")]:
            with self.subTest(outcome=outcome, kind=kind):
                start = self.start()
                for index in range(4):
                    result = self.poll(outcome, kind)
                    self.assertEqual(result["selection"], start["models"][(index + 1) % 4])
                    for selected in (start["models"][index], result["selection"]):
                        self.assertIn(f"{selected['model']} {selected['effort']}", result["notice"])
                    self.assertEqual(result["delay_ms"], 300000 if index == 3 else 1000)
                    self.assertEqual(self.driver.run.successes, 0)
                self.assertIn("cooldown 5 minutes", result["notice"])
                self.assertEqual(self.poll("provider_error", "transient")["delay_ms"], 2000)

    def test_transient_exhaustion_all_favorites_and_combined_limits(self):
        start = self.start()
        for index in range(4):
            for delay in (2000, 4000, 8000):
                result = self.poll("provider_error", "transient")
                self.assertEqual(result["selection"], start["models"][index])
                self.assertEqual(result["delay_ms"], delay)
            result = self.poll("provider_error", "transient")
            self.assertEqual(result["delay_ms"], 300000 if index == 3 else 1000)
        self.assertEqual(result["selection"], start["models"][0])
        self.start("--for", "1m", "--turns", "1")
        self.assertIn("turn limit", self.poll()["notice"])
        self.start("--turns", "100", "--for", "1m")
        self.clock.now += 60
        self.assertIn("Duration limit", self.poll()["notice"])

    def test_maximum_goal_and_favorites_fit_wire_and_prompt_bounds(self):
        pairs = [part for i in range(16) for part in (f"provider/model-{i}-" + "x" * 230, "ultracode")]
        self.command("favorites", "set", *pairs)
        result = self.command("start", "--", "🦉" * (plugin.MAX_GOAL // 4))
        self.assertLessEqual(len(plugin.json_bytes({"session_driver": result})), plugin.MAX_FRAME)
        self.assertLessEqual(len(result["prompt"].encode()), 16 * 1024)
        self.assertEqual(len(result["models"]), 16)
        self.assertLessEqual((self.root / "prefs.json").stat().st_size, plugin.MAX_PREFS)
        following = self.poll()
        self.assertLessEqual(len(following["prompt"].encode()), 16 * 1024)
        self.assertLessEqual(len(following["notice"].encode()), 2 * 1024)
        # Include RPC envelopes, maximum byte goals, escaping, and every recovery prompt.
        for goal in ("🦉" * (plugin.MAX_GOAL // 4), "\\" * plugin.MAX_GOAL,
                     "x" + "\n" * (plugin.MAX_GOAL - 2) + "x"):
            result = self.command("start", "--", goal)
            results = [result, self.poll(), self.poll("provider_error", "transient"),
                       self.poll("provider_error", "auth"), self.poll("selection_rejected", "unknown")]
            for _ in range(3):
                results.append(self.poll(feedback="repeated"))
            for result in results:
                self.assertLessEqual(len(result["prompt"].encode()), 16 * 1024)
                self.assertLessEqual(len(result["notice"].encode()), 2 * 1024)
                message = {"jsonrpc": "2.0", "id": "x" * 128, "result": {"session_driver": result}}
                self.assertLessEqual(len(plugin.json_bytes(message)), plugin.MAX_FRAME)
                self.assertEqual(plugin.read_message(io.BytesIO(framed(message))), message)

    def test_retries_and_cooldown_cannot_outlive_deadline(self):
        self.start("--for", "1m")
        self.clock.now += 58
        self.assertEqual(self.poll("provider_error", "transient")["action"], "stop")
        self.start("--for", "1m")
        for _ in range(3):
            self.assertEqual(self.poll("provider_error", "quota")["action"], "next")
        self.assertEqual(self.poll("provider_error", "quota")["action"], "stop")

    def test_blocked_unknown_and_inconsistent_callbacks_stop_without_counting(self):
        for outcome, kind in [("blocked", "unknown"), ("blocked", "auth"), ("provider_error", "unknown"),
                              ("provider_error", "none"), ("success", "auth"), ("unknown", "unknown"),
                              ("success", "arbitrary error text")]:
            with self.subTest(outcome=outcome, kind=kind):
                self.start()
                run = self.driver.run
                self.assertEqual(self.poll(outcome, kind)["action"], "stop")
                self.assertEqual(run.successes, 0)
                self.assertIsNone(self.driver.run)

    def test_run_and_last_emitted_selection_must_match_before_counting(self):
        for updates in ({"run_id": "other"}, {"model": "x-ai/grok-4.6"}, {"effort": "ultracode"},
                        {"model": "evil/model"}, {"effort": "max"}):
            self.start()
            run = self.driver.run
            self.assertEqual(self.poll(**updates)["action"], "stop")
            self.assertEqual(run.successes, 0)
        self.start()
        old = poll_request(self.driver)
        self.poll("provider_error", "auth")
        self.assertEqual(send_poll(self.driver, old)["action"], "stop")

    def test_duplicate_latest_decision_is_idempotent_even_after_failover(self):
        self.start("--turns", "2")
        request = poll_request(self.driver, decision="decision-1")
        first = send_poll(self.driver, request)
        for _ in range(5):
            self.assertEqual(send_poll(self.driver, request), first)
            self.assertEqual(self.driver.run.successes, 1)
        self.assertEqual(self.poll(decision="decision-2")["action"], "stop")
        self.assertEqual(send_poll(self.driver, request)["action"], "stop")
        self.start()
        request = poll_request(self.driver, "provider_error", "auth", decision="account-1")
        first = send_poll(self.driver, request)
        for _ in range(4):
            self.assertEqual(send_poll(self.driver, request), first)
            self.assertEqual(self.driver.run.index, 1)

    def test_duplicate_cannot_bypass_expired_or_delayed_deadline(self):
        self.start("--for", "1m")
        request = poll_request(self.driver)
        self.assertEqual(send_poll(self.driver, request)["action"], "next")
        self.clock.now += 59
        self.assertEqual(send_poll(self.driver, request)["action"], "stop")

    def test_changed_or_stale_decision_is_rejected(self):
        self.start()
        request = poll_request(self.driver, decision="stable-id")
        send_poll(self.driver, request)
        request["outcome"] = "provider_error"
        request["error_kind"] = "transient"
        self.assertEqual(send_poll(self.driver, request)["action"], "stop")
        self.start()
        request = poll_request(self.driver, decision="stale-id")
        send_poll(self.driver, request)
        self.poll()
        self.assertEqual(send_poll(self.driver, request)["action"], "stop")

    def test_lru_is_bounded_and_returned_results_cannot_mutate_cache(self):
        self.start()
        for i in range(plugin.CACHE_SIZE + 8):
            self.poll(decision=f"decision-{i}")
        self.assertEqual(len(self.driver.cache), plugin.CACHE_SIZE)
        self.assertNotIn("decision-0", self.driver.cache)
        request = poll_request(self.driver, decision="latest")
        result = send_poll(self.driver, request)
        result["selection"]["effort"] = "off"
        self.assertEqual(send_poll(self.driver, request)["selection"]["effort"], "ultra")
        self.assertEqual(self.driver.run.successes, plugin.CACHE_SIZE + 9)

    def test_malformed_polls_stop(self):
        for update in ({"decision_id": "bad id"}, {"decision_id": "x" * 129}, {"decision_id": "é"},
                       {"decision_id": ""}, {"outcome": []}, {"extra": "field"}, {"model": None}):
            self.start()
            self.assertEqual(self.poll(**update)["action"], "stop")
        for args in ([], ["{}", "{}"], ["null"], ["[]"], ["{"], ["[" * 40 + "]" * 40],
                     ["x" * 4097], [True], {}, ["\ud800"]):
            self.start()
            self.assertEqual(self.driver.poll(args)["session_driver"]["action"], "stop")

    def test_stop_restart_reinitialize_and_new_explicit_start(self):
        first = self.start()
        request = poll_request(self.driver)
        self.assertEqual(self.command("stop")["action"], "stop")
        self.assertEqual(send_poll(self.driver, request)["action"], "stop")
        second = self.start()
        self.assertNotEqual(first["run_id"], second["run_id"])
        self.driver.initialize({})
        self.assertIsNone(self.driver.run)
        self.assertEqual(len(self.driver.cache), 0)
        self.assertEqual(send_poll(self.driver, request)["action"], "stop")
        restarted = plugin.Driver(self.root, self.clock)
        restarted.initialize({})
        self.assertEqual(send_poll(restarted, request)["action"], "stop")
        self.assertEqual(list(self.root.iterdir()), [])

    def test_favorite_schema_and_exact_canonical_efforts(self):
        for effort in sorted(plugin.EFFORTS):
            self.assertEqual(plugin.favorites([{"model": "provider/model", "effort": effort}])[0]["effort"], effort)
        bad = [None, {}, [], [None], [{"model": "a/b", "effort": "ultra", "extra": 1}],
               [{"model": "a/b"}], [{"model": "a/b", "effort": True}],
               [{"model": "a/b", "effort": "ultra"}] * 2,
               [{"model": f"a/b{i}", "effort": "high"} for i in range(17)]]
        bad.extend([{"model": model, "effort": "high"}] for model in
                   ["unqualified", "/model", "provider/", "a//b", "../b", "a/..", " a/b", "a/b ", "a/b\n", "a/é", "a/" + "x" * 255])
        bad.extend([{"model": "a/b", "effort": effort}] for effort in
                   ["Ultra", "none", "med", "x-high", "x_high", "max ", " ultra", "unknown"])
        for value in bad:
            with self.subTest(value=value):
                with self.assertRaises(plugin.Invalid):
                    plugin.favorites(value)

    def test_preferences_persist_only_choices_privately_and_pin_run_models(self):
        start = self.start()
        self.command("favorites", "set", "custom/model", "ultra", "other/model", "max")
        self.assertEqual(self.driver.run.models, start["models"])
        path = self.root / "prefs.json"
        self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        value = json.loads(path.read_text())
        self.assertEqual(set(value), {"version", "favorites"})
        self.assertNotIn("Inspect", path.read_text())
        restarted = plugin.Driver(self.root)
        restarted.initialize({})
        self.assertIsNone(restarted.run)
        self.assertEqual(restarted.choices, value["favorites"])
        new = self.start()
        self.assertEqual(new["models"], value["favorites"])
        self.command("favorites", "reset")
        self.assertEqual(self.driver.choices, plugin.defaults())
        self.assertEqual(json.loads(path.read_text())["favorites"], plugin.defaults())
        self.assertEqual(sorted(p.name for p in self.root.iterdir()), ["prefs.json"])

    def test_invalid_prefs_never_silently_fallback_or_start(self):
        path = self.root / "prefs.json"
        for body in (b"{", b"{}", b'{"version":true,"favorites":[]}', b'{"version":2,"favorites":[]}',
                     b'{"version":1,"favorites":[],"run":{}}', b'{"version":1,"version":1,"favorites":[]}',
                     b'{"version":1,"favorites":[]}', b"x" * (plugin.MAX_PREFS + 1)):
            path.write_bytes(body)
            path.chmod(0o600)
            self.driver.initialize({})
            self.assertIsNone(self.driver.choices)
            self.assertIn("unsafe", self.command("status")["notice"])
            with self.assertRaises(plugin.Invalid):
                self.start()
        # A safe, regular, bounded but malformed schema may be explicitly reset.
        path.write_bytes(b"{}")
        self.command("favorites", "reset")
        self.assertEqual(self.driver.choices, plugin.defaults())

    def test_preferences_refuse_symlinks_hardlinks_permissions_and_special_files(self):
        path = self.root / "prefs.json"
        target = self.root / "target"
        target.write_bytes(b"unchanged")
        target.chmod(0o600)
        path.symlink_to(target)
        with self.assertRaises(plugin.Invalid):
            self.command("favorites", "reset")
        self.assertTrue(path.is_symlink())
        self.assertEqual(target.read_bytes(), b"unchanged")
        path.unlink()
        os.link(target, path)
        with self.assertRaises(plugin.Invalid):
            self.command("favorites", "reset")
        path.unlink()
        path.write_bytes(b"{}")
        path.chmod(0o644)
        with self.assertRaises(plugin.Invalid):
            self.command("favorites", "reset")
        path.unlink()
        path.mkdir()
        with self.assertRaises(plugin.Invalid):
            self.command("favorites", "reset")
        path.rmdir()
        if hasattr(os, "mkfifo"):
            os.mkfifo(path, 0o600)
            with self.assertRaises(plugin.Invalid):
                self.command("favorites", "reset")
            path.unlink()
        self.root.chmod(0o777)
        try:
            with self.assertRaises(plugin.Invalid):
                self.command("favorites", "reset")
        finally:
            self.root.chmod(0o700)
        alias = self.root / "alias"
        alias.symlink_to(self.root, target_is_directory=True)
        with self.assertRaises(OSError):
            plugin.Preferences(alias).load()

    def test_atomic_write_failure_leaves_old_preferences_and_cleans_temp(self):
        self.command("favorites", "reset")
        path = self.root / "prefs.json"
        before = path.read_bytes()
        with patch.object(plugin.os, "replace", side_effect=OSError("fixture failure")):
            with self.assertRaises(plugin.Invalid):
                self.command("favorites", "set", "other/model", "high")
        self.assertEqual(path.read_bytes(), before)
        self.assertEqual(self.driver.choices, plugin.defaults())
        self.assertEqual([p.name for p in self.root.iterdir()], ["prefs.json"])


class WireTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        shutil.copyfile(ROOT / "main.py", self.root / "main.py")
        self.processes = []
        self.addCleanup(self.close_processes)

    def close_processes(self):
        for process in self.processes:
            if process.poll() is None:
                process.kill()
            process.communicate(timeout=5)

    def process(self):
        env = {"PATH": os.defpath, "HOME": str(self.root), "PYTHONDONTWRITEBYTECODE": "1"}
        process = subprocess.Popen([sys.executable, "main.py"], cwd=self.root, env=env,
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, bufsize=0)
        self.processes.append(process)
        return process

    def receive(self, process):
        # Read readiness for *every* partial byte/body, so broken framing cannot hang tests.
        class TimedReader:
            def read(self, count):
                with selectors.DefaultSelector() as selector:
                    selector.register(process.stdout, selectors.EVENT_READ)
                    if not selector.select(5):
                        raise AssertionError("plugin response timed out")
                return os.read(process.stdout.fileno(), count)

            def readline(self, count):
                line = bytearray()
                while len(line) < count:
                    byte = self.read(1)
                    if not byte:
                        break
                    line.extend(byte)
                    if byte == b"\n":
                        break
                return bytes(line)
        result = plugin.read_message(TimedReader())
        self.assertIsNotNone(result, "plugin closed stdout unexpectedly")
        return result

    def rpc(self, process, method, params=None, rid=1):
        process.stdin.write(framed({"jsonrpc": "2.0", "id": rid, "method": method, "params": params or {}}))
        frames = []
        while True:
            response = self.receive(process)
            if "id" in response:
                self.assertEqual(response["id"], rid)
                return response, frames
            frames.append(response)

    def command(self, process, args, command="auto", rid=2):
        return self.rpc(process, "command.invoke", {"command": command, "args": args,
                        "request_id": "wire-request-" + str(rid)}, rid)

    def start(self, process):
        self.rpc(process, "initialize", {"extension_protocol_version": 1, "plugin_root": str(self.root)})
        response, _ = self.command(process, ["start", "--turns", "2", "--", "Inspect", "🦉", "only"])
        return response["result"]["session_driver"]

    def test_real_protocol_initialization_never_arms_from_notifications_hooks_tools(self):
        process = self.process()
        response, _ = self.command(process, ["start", "--", "g"])
        self.assertIn("error", response)
        response, frames = self.rpc(process, "initialize", {"extension_protocol_version": 1})
        self.assertEqual(frames, [])
        self.assertEqual(response["result"], {"protocol_version": 1, "capabilities": {"tools": [], "providers": []}})
        for method in ("tool.call", "hook.handle"):
            response, _ = self.rpc(process, method, {"name": "auto", "command": "auto", "args": ["start", "--", "g"]})
            self.assertNotIn("session_driver", response.get("result", {}))
        process.stdin.write(framed({"jsonrpc": "2.0", "method": "command.invoke", "params": {
            "command": "auto", "args": ["start", "--", "g"], "request_id": "notification"}}))
        response, _ = self.command(process, ["status"])
        self.assertIn("No active run", response["result"]["session_driver"]["notice"])
        response, _ = self.command(process, ["{}"], "__session_driver__")
        self.assertEqual(response["result"]["session_driver"]["action"], "stop")
        self.assertFalse((self.root / "prefs.json").exists())

    def test_real_framing_duplicate_decisions_and_limit(self):
        process = self.process()
        start = self.start(process)
        self.assertEqual(start["models"], plugin.defaults())
        request = {"run_id": start["run_id"], "decision_id": "decision-1", "outcome": "success",
                   "error_kind": "none", **start["models"][0]}
        first, frames = self.command(process, [json.dumps(request)], "__session_driver__", rid=3)
        self.assertEqual(frames, [])
        for rid in (4, 5):
            duplicate, _ = self.command(process, [json.dumps(request)], "__session_driver__", rid=rid)
            self.assertEqual(duplicate["result"], first["result"])
        self.assertEqual(first["result"]["session_driver"]["action"], "next")
        self.assertIn("1/2", first["result"]["session_driver"]["notice"])
        self.assertIn("Inspect 🦉 only", first["result"]["session_driver"]["prompt"])
        request["decision_id"] = "decision-2"
        response, _ = self.command(process, [json.dumps(request)], "__session_driver__")
        self.assertEqual(response["result"]["session_driver"]["action"], "stop")

    def test_real_wire_context_default_auto_off_and_start_only(self):
        process = self.process()
        self.rpc(process, "initialize", {"extension_protocol_version": 1})
        for flags, mode in (([], "auto"), (["--context", "auto"], "auto"),
                            (["--context", "off"], "off")):
            with self.subTest(flags=flags):
                response, frames = self.command(process, ["start", "--for", "1h", *flags,
                                                         "--turns", "2", "--", "new prompt goal --context off"])
                self.assertEqual(frames, [])
                start = response["result"]["session_driver"]
                self.assertEqual(set(start), {"action", "run_id", "models", "prompt", "delay_ms", "notice",
                                              "feedback_version", "time_checkpoint_version", "max_duration_ms", "context_mode"})
                self.assertEqual(start["context_mode"], mode)
                self.assertEqual(start["max_duration_ms"], 3_600_000)
                self.assertIn(f"Context mode: {mode}", start["notice"])
                self.assertTrue(start["prompt"].endswith("\nnew prompt goal --context off"))
                request = {"run_id": start["run_id"], "decision_id": "context-1", "outcome": "success",
                           "error_kind": "none", **start["models"][0], "feedback": "changed"}
                response, frames = self.command(process, [json.dumps(request)], "__session_driver__")
                self.assertEqual(frames, [])
                result = response["result"]["session_driver"]
                self.assertEqual(set(result), {"action", "run_id", "selection", "prompt", "delay_ms", "notice"})
                self.assertEqual(result["action"], "next")
                request.update(decision_id="context-2", context_mode=mode)
                response, frames = self.command(process, [json.dumps(request)], "__session_driver__")
                self.assertEqual(frames, [])
                self.assertEqual(response["result"]["session_driver"]["action"], "stop")
        self.assertEqual(list(self.root.iterdir()), [self.root / "main.py"])

    def test_real_wire_invalid_context_flags_cannot_start(self):
        process = self.process()
        self.rpc(process, "initialize")
        for args in (["--context"], ["--context", "--", "goal"],
                     ["--context", "invalid", "--", "goal"],
                     ["--context", "off", "--context", "auto", "--", "goal"],
                     ["--context", "auto", "goal"], ["--context=off", "--", "goal"]):
            with self.subTest(args=args):
                response, _ = self.command(process, ["start", *args])
                self.assertIn("error", response)
                self.assertNotIn("result", response)
                response, _ = self.command(process, ["status"])
                status = response["result"]["session_driver"]
                self.assertEqual(set(status), {"action", "notice"})
                self.assertIn("No active run", status["notice"])
        self.assertEqual(self.start(process)["context_mode"], "auto")
        self.assertEqual(list(self.root.iterdir()), [self.root / "main.py"])

    def test_real_wire_feedback_opt_in_threshold_duplicate_and_limit(self):
        process = self.process()
        self.rpc(process, "initialize", {"extension_protocol_version": 1})
        response, frames = self.command(process, ["start", "--turns", "5", "--", "Inspect 🦉 only"])
        self.assertEqual(frames, [])
        start = response["result"]["session_driver"]
        self.assertIs(type(start["feedback_version"]), int)
        self.assertEqual(start["feedback_version"], 1)
        self.assertEqual(start["context_mode"], "auto")
        self.assertEqual(set(start), {"action", "run_id", "models", "prompt", "delay_ms", "notice",
                                      "feedback_version", "time_checkpoint_version", "context_mode"})
        selected = start["models"][0]
        for count, label in enumerate(("changed", "repeated", "empty", "repeated", "repeated"), 1):
            request = {"run_id": start["run_id"], "decision_id": f"feedback-{count}",
                       "outcome": "success", "error_kind": "none", **selected, "feedback": label}
            response, frames = self.command(process, [json.dumps(request)], "__session_driver__", rid=count + 2)
            self.assertEqual(frames, [])
            result = response["result"]["session_driver"]
            if count == 5:
                self.assertEqual(result["action"], "stop")
                self.assertIn("5/5", result["notice"])
                continue
            self.assertEqual(result["action"], "next")
            self.assertNotIn("feedback_version", result)
            selected = start["models"][0 if count < 4 else 1]
            self.assertEqual(result["selection"], selected)
            self.assertIn(f"{count}/5", result["notice"])
            duplicate, frames = self.command(process, [json.dumps(request)], "__session_driver__", rid=count + 10)
            self.assertEqual(frames, [])
            self.assertEqual(duplicate["result"], response["result"])
            if count in (2, 3, 4):
                self.assertIn("If you are stuck requesting a confirmation phrase", result["prompt"])
                self.assertIn("Repetition or a model switch supplies no approval", result["prompt"])
            if count in (2, 3):
                self.assertIn("correcting course on the same favorite", result["notice"])
            if count == 4:
                for selection in start["models"][:2]:
                    self.assertIn(f"{selection['model']} {selection['effort']}", result["notice"])
                self.assertIn("feedback suggests repetition/stalling or empty output", result["prompt"])
                self.assertIn("Never replay side effects or bypass gates", result["prompt"])
        self.assertEqual(list(self.root.iterdir()), [self.root / "main.py"])

    def test_real_wire_feedback_resets_legacy_provider_retry_and_cooldown(self):
        process = self.process()
        self.rpc(process, "initialize")
        self.command(process, ["favorites", "set", "custom/model", "ultracode"])
        response, _ = self.command(process, ["start", "--for", "1h", "--", "Inspect only"])
        start = response["result"]["session_driver"]
        selection = start["models"][0]
        request = {"run_id": start["run_id"], "decision_id": "unused", "outcome": "success",
                   "error_kind": "none", **selection}
        sequence = [("success", "none", label, 1000) for label in
                    ("repeated", "empty", "unknown", "repeated", "empty", None, "repeated", "empty")]
        sequence += [("provider_error", "transient", "unknown", 2000)]
        sequence += [("success", "none", label, delay) for label, delay in
                     (("empty", 1000), ("repeated", 1000), ("empty", 300000))]
        for index, (outcome, kind, label, delay) in enumerate(sequence):
            request.update(decision_id=f"mixed-{index}", outcome=outcome, error_kind=kind)
            request.pop("feedback", None)
            if label is not None:
                request["feedback"] = label
            response, frames = self.command(process, [json.dumps(request)], "__session_driver__", rid=index + 3)
            self.assertEqual(frames, [])
            result = response["result"]["session_driver"]
            self.assertEqual(result["action"], "next")
            self.assertEqual(result["selection"], selection)
            self.assertEqual(result["delay_ms"], delay)
            if outcome == "provider_error":
                self.assertIn("previous provider attempt failed", result["prompt"])
        self.assertIn("cooldown 5 minutes", result["notice"])
        self.assertIn("11/unbounded", result["notice"])
        duplicate, _ = self.command(process, [json.dumps(request)], "__session_driver__")
        self.assertEqual(duplicate["result"], response["result"])
        # Reinitialization during cooldown never resumes or replays that decision.
        self.rpc(process, "initialize")
        response, _ = self.command(process, [json.dumps(request)], "__session_driver__")
        self.assertEqual(response["result"]["session_driver"]["action"], "stop")

    def test_real_wire_invalid_optional_feedback_and_changed_duplicate_fail_closed(self):
        process = self.process()
        for outcome, kind, label in (("success", "none", None), ("success", "none", True),
                                     ("success", "none", "raw secret error text"),
                                     ("provider_error", "auth", "empty"),
                                     ("selection_rejected", "unknown", "changed"),
                                     ("blocked", "unknown", "repeated")):
            start = self.start(process)
            request = {"run_id": start["run_id"], "decision_id": "invalid-feedback",
                       "outcome": outcome, "error_kind": kind, **start["models"][0], "feedback": label}
            response, frames = self.command(process, [json.dumps(request)], "__session_driver__")
            self.assertEqual(frames, [])
            result = response["result"]["session_driver"]
            self.assertEqual(result["action"], "stop")
            self.assertIn("0/2", result["notice"])
            self.assertNotIn("raw secret error text", json.dumps(response))
        start = self.start(process)
        request = {"run_id": start["run_id"], "decision_id": "feedback-changed", "outcome": "success",
                   "error_kind": "none", **start["models"][0]}
        self.command(process, [json.dumps(request)], "__session_driver__")
        request["feedback"] = "unknown"
        response, _ = self.command(process, [json.dumps(request)], "__session_driver__")
        self.assertEqual(response["result"]["session_driver"]["action"], "stop")
        self.assertIn("changed payload", response["result"]["session_driver"]["notice"])

    def test_real_failover_mismatch_and_process_restart(self):
        process = self.process()
        start = self.start(process)
        request = {"run_id": start["run_id"], "decision_id": "rejected-1", "outcome": "selection_rejected",
                   "error_kind": "unknown", **start["models"][0]}
        response, _ = self.command(process, [json.dumps(request)], "__session_driver__")
        result = response["result"]["session_driver"]
        self.assertEqual(result["selection"], start["models"][1])
        self.assertIn("Inspect 🦉 only", result["prompt"])
        request.update(decision_id="wrong-selection", outcome="success", error_kind="none")
        response, _ = self.command(process, [json.dumps(request)], "__session_driver__")
        self.assertEqual(response["result"]["session_driver"]["action"], "stop")
        self.command(process, ["favorites", "set", "custom/model", "ultra"])
        start = self.command(process, ["start", "--", "not persisted"])[0]["result"]["session_driver"]
        process.kill()
        process.communicate(timeout=5)
        self.processes.remove(process)
        restarted = self.process()
        self.rpc(restarted, "initialize", {"extension_protocol_version": 1})
        request.update(run_id=start["run_id"], **start["models"][0])
        response, _ = self.command(restarted, [json.dumps(request)], "__session_driver__")
        self.assertEqual(response["result"]["session_driver"]["action"], "stop")
        response, frames = self.command(restarted, ["favorites"])
        self.assertEqual([frame["method"] for frame in frames], ["command.output", "command.output"])
        self.assertEqual(frames[0]["params"]["event"]["rows"], [["1", "custom/model", "ultra"]])
        self.assertEqual(frames[1]["params"]["event"]["kind"], "done")
        self.assertEqual(frames[0]["params"]["request_id"], "wire-request-2")
        self.assertNotIn("not persisted", (self.root / "prefs.json").read_text())

    def test_fragmented_coalesced_and_unicode_frames_shutdown(self):
        process = self.process()
        data = framed({"jsonrpc": "2.0", "id": "init", "method": "initialize", "params": {}})
        for byte in data:
            process.stdin.write(bytes([byte]))
        self.assertEqual(self.receive(process)["id"], "init")
        data = framed({"jsonrpc": "2.0", "id": "one", "method": "command.invoke", "params": {
            "command": "auto", "args": ["start", "--", "🦉 café"], "request_id": "unicode"}})
        data += framed({"jsonrpc": "2.0", "id": 3, "method": "shutdown"})
        process.stdin.write(data)
        response = self.receive(process)
        self.assertIn("🦉 café", response["result"]["session_driver"]["prompt"])
        self.assertEqual(self.receive(process), {"jsonrpc": "2.0", "id": 3, "result": None})
        output, errors = process.communicate(timeout=5)
        self.processes.remove(process)
        self.assertEqual((process.returncode, output, errors), (0, b"", b""))

    def test_hostile_frames_bounded_fail_closed_in_real_process(self):
        bodies = [b"{", b"[" * 33 + b"]" * 33, b'{"id":1,"id":2}', b'{"id":NaN}',
                  b'{"id":1e999}', b'{"id":' + b"9" * 1000 + b"}", b'"\xff"']
        invalid = [b"X" * 1025, b"X: a\r\n" * 700, b"\xff: x\r\n\r\n",
                   b"Content-Length: -1\r\n\r\n", b"Content-Length: 0\r\n\r\n",
                   b"Content-Length: 65537\r\n\r\n", b"Content-Length: 99999999999\r\n\r\n",
                   b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}", b"X: a\r\n\r\n{}",
                   b"bad header\r\n\r\n{}", b"Content-Length: 100\r\n\r\n{}",
                   b"Content-Length: 2\r\n", b"Content-Length: 2\x00\r\n\r\n{}"]
        invalid.extend(f"Content-Length: {len(body)}\r\n\r\n".encode() + body for body in bodies)
        for data in invalid:
            with self.subTest(data=data[:80]):
                process = self.process()
                output, errors = process.communicate(data, timeout=5)
                self.processes.remove(process)
                self.assertEqual(process.returncode, 1)
                self.assertEqual(output, b"")
                self.assertIn(b"run forgotten", errors)
                self.assertLess(len(errors), 128)

    def test_invalid_requests_cannot_arm_and_transport_stays_in_sync(self):
        process = self.process()
        self.rpc(process, "initialize")
        for value in ([], True, {"jsonrpc": "1.0", "id": 1, "method": "initialize"},
                      {"jsonrpc": "2.0", "id": True, "method": "initialize"},
                      {"jsonrpc": "2.0", "id": {}, "method": "initialize"}):
            process.stdin.write(framed(value))
            self.assertEqual(self.receive(process)["error"]["code"], -32600)
        response, _ = self.rpc(process, "command.invoke", {"command": "auto", "args": ["start", "--", "g"]})
        self.assertEqual(response["error"]["code"], -32602)
        response, _ = self.command(process, ["status"])
        self.assertEqual(response["result"]["session_driver"]["action"], "status")
        response, _ = self.command(process, ["{}"], "__session_driver__")
        self.assertEqual(response["result"]["session_driver"]["action"], "stop")

    def test_bounded_reads_lf_headers_and_duplicate_nested_json(self):
        self.assertIsNone(plugin.read_message(io.BytesIO()))
        message = {"jsonrpc": "2.0", "id": 1, "method": "info.get"}
        body = plugin.json_bytes(message)
        data = b"content-length: " + str(len(body)).encode() + b"\nContent-Type: application/json\n\n" + body
        self.assertEqual(plugin.read_message(io.BytesIO(data)), message)
        with self.assertRaises(plugin.Invalid):
            plugin.strict_json('{"selection":{"model":"a/b","model":"evil/model"}}')
        with self.assertRaises(plugin.FrameError):
            plugin.write_message(io.BytesIO(), {"too_large": "x" * plugin.MAX_FRAME})
        # Header/body caps are enforced before any unbounded read is requested.
        class Guarded(io.BytesIO):
            def read(self, count=-1):
                self.assert_bounded(count, plugin.MAX_FRAME)
                return super().read(min(count, 3))

            def readline(self, count=-1):
                self.assert_bounded(count, plugin.MAX_HEADER_LINE + 1)
                return super().readline(count)

            @staticmethod
            def assert_bounded(count, maximum):
                if not 0 <= count <= maximum:
                    raise AssertionError("unbounded read")
        self.assertEqual(plugin.read_message(Guarded(framed(message))), message)


if __name__ == "__main__":
    unittest.main()
