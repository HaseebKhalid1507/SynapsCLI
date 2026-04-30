# VM Automation Agent + virsh Skill — Implementation Plan

**Date:** 2026-04-30  
**Status:** Implemented locally; final Windows UIA bridge validation partially blocked by AHK subprocess timeout in live VM.  
**Goal:** Build a resident, cross-platform VM automation agent plus Synaps skills/scripts for controlling libvirt/virsh VMs and executing predictive desktop automation plans inside the guest.

## Summary

Synaps should not micromanage every GUI action through slow LLM observe/act loops. Instead, Synaps will:

1. Use a host-side `virsh` skill to manage VM lifecycle, snapshots, display, IP discovery, and guest-agent health.
2. Connect to a resident `synaps-vm-agent` running inside the guest.
3. Submit compact, executable behavior-tree-style plans to the agent.
4. Let the agent run local waits, selectors, retries, state checks, and recovery at machine speed.
5. Store successful traces, selectors, plans, and failures in a VM memory store modeled after the web skill's VelociRAG workflow.

Windows is first-class for v0. Linux/Unix compatibility is part of the architecture and gets a minimal backend after the core agent contract stabilizes. macOS is explicitly out of scope.

---

## Live validation status — 2026-04-30

Completed in local repos and deployed to `win11` VM:

- Agent package, HTTP API, auth, plan executor, process/fs ops, traces, Linux backend reporting, Windows AHK backend wrapper, host virsh plugin, skills, installer scripts, and smoke-test documentation.
- `win11` guest health is reachable at `http://192.168.122.146:8765/health` with token auth.
- Unauthorized guest-agent requests return HTTP 401; authenticated requests succeed.
- Process plans work in the guest; Notepad and Chrome launch in the interactive Windows session.
- Host `vmctl.py status win11` reports VM state, display URI, QEMU guest-agent health, guest IP, guest agent health, and compatible agent version.
- Local verification: `synaps-vm-agent` test suite passes (`51 passed`); `synaps-vm-automation` plugin tests pass (`5 passed`).

Known live blocker:

- Windows AHK scripts are installed and AutoHotkey v2 is on machine PATH, but live `ui.tree` and `ui.set_text` calls time out after 10s in the VM. The Python backend now maps AHK subprocess timeouts to structured `AHK_TIMEOUT` failures instead of leaving plan steps with no trace event. Full Notepad UIA text-entry validation remains blocked until the AHK execution/session issue is fixed.

Security note for current VM validation:

- The `win11` deployment is intentionally in dev mode: bind `0.0.0.0`, token `dev-secret`, `allow_exec=true`, and a dev firewall rule. Production/default operation should use localhost binding plus tunnel/port-forward and a generated secret token.

---

## Convergence decision

**convergence:** informed

Rationale: this is a medium/large feature with security-sensitive surfaces: remote command execution, guest HTTP agent, VM lifecycle operations, and desktop control. Human review is expected, but design bias and blind spots matter.

Fixed loop parameters if implementation uses convergence:

- **threshold:** `0.8`
- **axis_weights:** default code-review weights
- **max_fix_iterations:** `2`
- **max_total_calls:** `10`

If the human wants lower cost, amend this plan to `convergence: none` before implementation starts.

---

## Scope

### In scope

- Python-based `synaps-vm-agent` package for guest-side automation.
- HTTP JSON API for health, capabilities, observation, primitive actions, and plan execution.
- Behavior-tree/blackboard plan executor.
- Windows backend using AutoHotkey v2 + UIA-v2 bridge for accessibility-driven automation.
- Minimal Linux backend scaffolding with process/filesystem support and placeholder UI capabilities.
- Host-side virsh helper scripts and Synaps skill docs.
- VelociRAG-backed memory workflow for VM automation traces, failures, selectors, and learned plans.
- Security controls: token auth, bind defaults, allowlist, timeouts, step limits, audit logs.

### Out of scope for v0

- macOS backend.
- Full computer-vision model integration.
- Production installer signing.
- Bidirectional streaming UI debugger.
- Kernel-level input injection.
- Arbitrary remote shell enabled by default.
- Full marketplace packaging automation.

---

## Proposed repo layout

Target repository split:

```text
maha-media/synaps-vm-agent
  pyproject.toml
  README.md
  synaps_vm_agent/
    __init__.py
    server.py
    config.py
    security.py
    plans/
      __init__.py
      model.py
      executor.py
      blackboard.py
      trace.py
    ops/
      __init__.py
      process.py
      fs.py
      ui.py
      memory.py
    backends/
      __init__.py
      base.py
      windows_ahk.py
      linux_basic.py
    ahk/
      bridge.ahk
      ui_tree.ahk
      ui_find.ahk
      ui_click.ahk
      ui_set_text.ahk
    tests/
      test_health.py
      test_plan_executor.py
      test_security.py
      test_process_ops.py
    schema/
      plan-v0.schema.json
      openapi-v0.yaml

maha-media/synaps-skills/synaps-vm-automation/
  .synaps-plugin/plugin.json
  skills/
    virsh-vm/SKILL.md
    vm-agent/SKILL.md
    windows-uia/SKILL.md
  scripts/
    vmctl.py
    memory.py
    install-agent.ps1
    install-agent.sh
  examples/
    notepad-plan.json
    health-check.sh
```

Notes:

- `synaps-vm-agent` is the distributable guest-agent application and owns service install, security-sensitive runtime code, AHK bridge code, and Python tests.
- `synaps-skills/synaps-vm-automation` is the Synaps plugin folder and owns host-side `virsh` orchestration, memory helper scripts, and skill prompting.
- A temporary prototype may be staged in `assets/` only if explicitly needed for local integration, but the target implementation should happen in the separate repos above.

### Cross-repo contract and versioning

- The canonical machine-readable contracts live in `synaps-vm-agent/schema/`:
  - `plan-v0.schema.json` for behavior-tree plans.
  - `openapi-v0.yaml` for HTTP endpoints once the API reaches first usable shape.
- The Synaps plugin must not define an independent copy of the protocol. It may vendor generated examples or a pinned schema copy only with the source agent version recorded.
- `synaps-skills/synaps-vm-automation/.synaps-plugin/plugin.json` should declare a minimum compatible agent version, e.g. `x-synaps-vm-agent-min-version: "0.1.0"`.
- `vmctl.py status <vm>` should report both plugin version and guest agent version and fail with a clear compatibility error if the guest agent is too old for a requested operation.
- Breaking protocol changes require a new schema namespace/version (`plan-v1`, `/v1/...`) or an explicit compatibility shim.
- Local development should support adjacent checkouts:

  ```text
  ~/Projects/Maha-Media/synaps-vm-agent
  ~/Projects/Maha-Media/synaps-skills/synaps-vm-automation
  ```

  The plugin's scripts should accept `SYNAPS_VM_AGENT_DEV_ROOT` to locate local schemas/examples during development.

---

## Architecture

### Host side

Host-side plugin responsibilities:

- `virsh list --all`
- `virsh domstate <vm>`
- `virsh start <vm>`
- `virsh shutdown <vm> --mode=agent`
- `virsh reboot <vm> --mode=agent`
- `virsh domifaddr <vm> --source agent`
- `virsh domdisplay <vm>`
- `virsh snapshot-*`
- `virsh qemu-agent-command <vm> '{"execute":"guest-ping"}'`
- call guest agent API at `http://<guest-ip>:8765`

### Guest side

Guest agent responsibilities:

- process operations
- filesystem operations
- UI observation and action operations
- behavior-tree plan execution
- blackboard state management
- traces and audit logs
- security policy enforcement

### Plan execution model

Node statuses:

```text
SUCCESS | FAILURE | RUNNING | TIMEOUT | RETRY | BLOCKED
```

Core node types:

```text
action
sequence
selector
retry
wait_until
parallel_race
parallel_all
set_blackboard
condition
```

Core primitive operations:

```text
process.start
process.exec
process.exists
process.wait_exit

fs.exists
fs.read
fs.write
fs.listdir

ui.observe
ui.active_window
ui.list_windows
ui.tree
ui.exists
ui.find
ui.wait_window
ui.wait_element
ui.click
ui.invoke
ui.set_text
ui.get_text
ui.hotkey
ui.type
ui.screenshot
ui.wait_stable

memory.trace
```

---

## Dependency graph

```text
Plan/API contracts
  ├── Config + security policy
  ├── Trace + blackboard models
  │   └── Plan executor core
  │       ├── Process/filesystem ops
  │       ├── HTTP API
  │       └── UI backend interface
  │           ├── Windows AHK/UIA backend
  │           └── Linux basic backend
  ├── Host virsh scripts
  │   └── Synaps plugin skills
  └── Memory workflow
      └── Learned plan/selector use in skills
```

Implementation order follows the graph: contracts and safe local executor first, then OS backends, then virsh/plugin integration, then memory learning.

---

# Tasks

## Task 1: Define VM agent contract and package skeleton

**Description:** Create the Python package skeleton and document the HTTP/plan API contract without implementing privileged operations.

**Acceptance criteria:**

- [x] `synaps-vm-agent/pyproject.toml` exists with package metadata and test dependencies.
- [x] `synaps-vm-agent/README.md` documents install, dev run, service goals, and security warnings.
- [x] `synaps_vm_agent/plans/model.py` defines typed/dataclass models for plan nodes, statuses, operation results, and traces.
- [x] `schema/plan-v0.schema.json` validates the plan DSL v0, including node types, operation fields, timeouts, and blackboard references.
- [x] `schema/openapi-v0.yaml` exists as an initial API contract for `/health`, `/capabilities`, `/observe`, `/plan/run`, `/plan/status/{id}`, `/plan/trace/{id}`, and `/plan/cancel/{id}`; endpoints may be marked planned until implemented.
- [x] Contract examples include `/health`, `/capabilities`, `/observe`, and `/plan/run` payloads.
- [x] README and skill examples that include plan JSON are validated against `plan-v0.schema.json` in tests.

**Verification:**

- [x] `cd synaps-vm-agent && python -m pytest` succeeds, even if only skeleton tests exist.
- [x] `cd synaps-vm-agent && python -m pip install -e .` succeeds in a venv.
- [x] `cd synaps-vm-agent && python -m pytest tests/test_schema_examples.py` validates bundled examples against schema.
- [x] Manual check: README examples are valid JSON and match the schema.

**Dependencies:** None  
**Files likely touched:** `synaps-vm-agent/pyproject.toml`, `synaps-vm-agent/README.md`, `synaps-vm-agent/synaps_vm_agent/plans/model.py`, `synaps-vm-agent/schema/plan-v0.schema.json`, `synaps-vm-agent/schema/openapi-v0.yaml`, `synaps-vm-agent/tests/test_health.py`, `synaps-vm-agent/tests/test_schema_examples.py`  
**Scope:** M

---

## Task 2: Implement config loading and security policy

**Description:** Add configuration loading, token auth primitives, bind defaults, host allowlist model, and execution limits.

**Acceptance criteria:**

- [x] Config defaults bind to `127.0.0.1:8765`.
- [x] Auth token is required for non-local requests unless explicitly disabled in config.
- [x] Config exposes max plan runtime, max steps, max subprocess runtime, and `allow_exec`.
- [x] Unit tests cover missing config, explicit config, auth success, auth failure, and disabled auth in dev mode.

**Verification:**

- [x] `cd synaps-vm-agent && python -m pytest tests/test_security.py` succeeds.
- [x] Manual check: invalid JSON/YAML config fails closed with a clear error.

**Dependencies:** Task 1  
**Files likely touched:** `config.py`, `security.py`, `tests/test_security.py`  
**Scope:** S

---

## Task 3: Build minimal HTTP server with health and capabilities

**Description:** Expose the first live agent API endpoints with auth enforcement and structured errors.

**Acceptance criteria:**

- [x] `GET /health` returns status, OS, version, and uptime.
- [x] `GET /capabilities` returns available backend capabilities.
- [x] Unauthorized requests return HTTP 401/403 with structured JSON.
- [x] Server can run with `python -m synaps_vm_agent.server`.

**Verification:**

- [x] `cd synaps-vm-agent && python -m pytest tests/test_health.py tests/test_security.py` succeeds.
- [x] Manual check: run server locally and `curl http://127.0.0.1:8765/health` returns JSON.

**Dependencies:** Task 2  
**Files likely touched:** `server.py`, `config.py`, `security.py`, `tests/test_health.py`  
**Scope:** M

---

## Task 4: Implement blackboard and trace recording

**Description:** Add task-local blackboard storage and structured trace events for every plan step.

**Acceptance criteria:**

- [x] Blackboard supports set/get by key and `$key` references in plan args.
- [x] Trace captures step id, op, args summary, status, duration, result summary, and error code.
- [x] Trace can be serialized to JSONL.
- [x] Tests cover blackboard reference resolution and trace serialization.

**Verification:**

- [x] `cd synaps-vm-agent && python -m pytest tests/test_plan_executor.py` succeeds.
- [x] Manual check: sample trace contains no auth token or secret config values.

**Dependencies:** Task 1  
**Files likely touched:** `plans/blackboard.py`, `plans/trace.py`, `plans/model.py`, `tests/test_plan_executor.py`  
**Scope:** S

---

## Task 5: Implement behavior-tree plan executor core

**Description:** Implement local plan execution for non-UI mock operations: `action`, `sequence`, `selector`, `retry`, `wait_until`, and `set_blackboard`.

**Acceptance criteria:**

- [x] `sequence` stops on first failure.
- [x] `selector` stops on first success.
- [x] `retry` retries failure with configured delay and attempt count.
- [x] `wait_until` polls a condition until success or timeout.
- [x] Executor enforces max steps and max runtime.

**Verification:**

- [x] `cd synaps-vm-agent && python -m pytest tests/test_plan_executor.py` succeeds.
- [x] Manual check: a sample plan produces a readable trace with expected statuses.

**Dependencies:** Tasks 2, 4  
**Files likely touched:** `plans/executor.py`, `plans/model.py`, `plans/blackboard.py`, `plans/trace.py`, `tests/test_plan_executor.py`  
**Scope:** M

---

## Checkpoint: After Tasks 1-5

- [x] Agent package installs.
- [x] Server health endpoint works.
- [x] Plan executor runs mock/local plans.
- [x] Security defaults fail closed.
- [x] Human reviews API/plan contract before OS automation begins.

---

## Task 6: Add process and filesystem primitive operations

**Description:** Add safe process and filesystem operations behind the executor operation registry.

**Acceptance criteria:**

- [x] `process.start`, `process.exec`, `process.exists`, and `process.wait_exit` work for simple commands.
- [x] `fs.exists`, `fs.read`, `fs.write`, and `fs.listdir` work in a configurable sandbox root when set.
- [x] `allow_exec=false` blocks process operations with a structured policy error.
- [x] Subprocess timeouts are enforced.

**Verification:**

- [x] `cd synaps-vm-agent && python -m pytest tests/test_process_ops.py` succeeds.
- [x] Manual check: sample plan starts a harmless process/command and records trace output.

**Dependencies:** Task 5  
**Files likely touched:** `ops/process.py`, `ops/fs.py`, `plans/executor.py`, `tests/test_process_ops.py`  
**Scope:** M

---

## Task 7: Expose plan run/status/cancel endpoints

**Description:** Wire the executor into HTTP endpoints for asynchronous plan execution.

**Acceptance criteria:**

- [x] `POST /plan/run` starts a plan and returns `plan_id`.
- [x] `GET /plan/status/{plan_id}` returns current status and latest trace summary.
- [x] `GET /plan/trace/{plan_id}` returns full trace.
- [x] `POST /plan/cancel/{plan_id}` requests cancellation.
- [x] Completed plans are retained for a configurable recent history count.

**Verification:**

- [x] `cd synaps-vm-agent && python -m pytest` succeeds.
- [x] Manual check: run a sample sleep/wait plan, poll status, cancel it, and inspect trace.

**Dependencies:** Task 6  
**Files likely touched:** `server.py`, `plans/executor.py`, `plans/trace.py`, `config.py`, `tests/test_health.py`, `tests/test_plan_executor.py`  
**Scope:** M

---

## Task 8: Define UI backend interface and basic observe API

**Description:** Add the OS-neutral UI backend interface and implement minimal observation using available platform metadata.

**Acceptance criteria:**

- [x] `backends/base.py` defines methods for active window, list windows, tree, find, click, set_text, hotkey, screenshot, and wait_stable.
- [x] `ops/ui.py` maps primitive op names to backend methods.
- [x] Unsupported operations return `CAPABILITY_UNAVAILABLE`, not generic failure.
- [x] `/observe` returns process/UI summary where available and capability gaps where not.

**Verification:**

- [x] `cd synaps-vm-agent && python -m pytest` succeeds.
- [x] Manual check: `/capabilities` accurately reflects the active backend.

**Dependencies:** Task 7  
**Files likely touched:** `backends/base.py`, `backends/linux_basic.py`, `backends/windows_ahk.py`, `ops/ui.py`, `server.py`  
**Scope:** M

---

## Task 9: Add Windows AutoHotkey/UIA bridge scripts

**Description:** Add AHK v2 scripts that expose Windows UI Automation operations through JSON input/output files.

**Acceptance criteria:**

- [x] `ui_tree.ahk` dumps active/window-specific accessibility tree as JSON.
- [x] `ui_find.ahk` finds elements by role/name/automation id/class/title filters.
- [x] `ui_click.ahk` clicks or invokes a found element.
- [x] `ui_set_text.ahk` sets text via UIA value pattern or keyboard fallback.
- [x] Scripts return structured errors on element not found or UIA failure.

**Verification:**

- [x] Manual Windows VM check: dump Notepad tree.
- [x] Manual Windows VM check: find editor and set text in Notepad.
- [x] Manual Windows VM check: click a known button in a simple dialog.

**Dependencies:** Task 8  
**Files likely touched:** `ahk/bridge.ahk`, `ahk/ui_tree.ahk`, `ahk/ui_find.ahk`, `ahk/ui_click.ahk`, `ahk/ui_set_text.ahk`, `README.md`  
**Scope:** M

---

## Task 10: Implement Windows backend wrapper

**Description:** Connect Python UI operations to the AHK/UIA scripts and expose Windows UI primitives through the plan executor.

**Acceptance criteria:**

- [x] Python backend detects AutoHotkey executable and UIA script availability.
- [x] `ui.active_window`, `ui.tree`, `ui.find`, `ui.click`, `ui.set_text`, `ui.hotkey`, and `ui.type` work on Windows.
- [x] Backend cleans up temporary JSON files.
- [x] AHK execution timeout is enforced.
- [x] Plan executor can open Notepad, wait for editor, and set text using UIA.

**Verification:**

- [x] Manual Windows VM check: run the Notepad sample plan end-to-end.
- [x] `cd synaps-vm-agent && python -m pytest` still succeeds on non-Windows with Windows tests skipped.

**Dependencies:** Task 9  
**Files likely touched:** `backends/windows_ahk.py`, `ops/ui.py`, `plans/executor.py`, `tests/test_plan_executor.py`  
**Scope:** M

---

## Checkpoint: After Tasks 6-10

- [x] Guest agent can run local process/fs plans.
- [x] Guest agent can run at least one Windows UIA plan end-to-end.
- [x] Plan traces are usable for debugging.
- [x] Security policy remains enforced for HTTP and process execution.
- [x] Human reviews Windows VM behavior before adding host virsh integration.

---

## Task 11: Implement host-side virsh helper script

**Description:** Add a host-side `vmctl.py` helper that wraps safe virsh operations and guest-agent calls with JSON output.

**Acceptance criteria:**

- [x] `vmctl.py list` returns VM names/states as JSON.
- [x] `vmctl.py status <vm>` returns domain state, IP candidates, guest-agent ping status, and display URI if available.
- [x] `vmctl.py start|shutdown|reboot <vm>` perform lifecycle operations with safe defaults.
- [x] `vmctl.py snapshot-create|snapshot-list|snapshot-revert` wrap virsh snapshot operations.
- [x] `vmctl.py agent-call <vm> <endpoint>` calls the in-guest agent after resolving IP.

**Verification:**

- [x] Manual host check: `python synaps-skills/synaps-vm-automation/scripts/vmctl.py list` returns JSON.
- [x] Manual host check against Windows VM: status resolves IP and guest agent health.
- [x] Manual host check: snapshot list works on a test VM.

**Dependencies:** Task 7  
**Files likely touched:** `synaps-skills/synaps-vm-automation/scripts/vmctl.py`, `synaps-skills/synaps-vm-automation/README.md`  
**Scope:** M

---

## Task 12: Create Synaps plugin manifest and virsh skill

**Description:** Package host-side VM operations as a Synaps plugin skill with clear LLM instructions and safe operating rules.

**Acceptance criteria:**

- [x] `.synaps-plugin/plugin.json` describes the plugin and skills.
- [x] `skills/virsh-vm/SKILL.md` teaches the model to use `vmctl.py` for lifecycle, IP, display, snapshots, and guest-agent calls.
- [x] Skill requires confirmation for destructive operations: hard poweroff, reset, snapshot delete, disk mutation.
- [x] Skill instructs the model to batch predictable operations and avoid unnecessary observe loops.

**Verification:**

- [x] Manual check: plugin manifest parses according to existing Synaps plugin schema.
- [x] Manual check: skill doc contains examples for start/status/snapshot/agent-call.

**Dependencies:** Task 11  
**Files likely touched:** `synaps-skills/synaps-vm-automation/.synaps-plugin/plugin.json`, `synaps-skills/synaps-vm-automation/skills/virsh-vm/SKILL.md`  
**Scope:** S

---

## Task 13: Create VM agent skill for predictive plans

**Description:** Add a skill that teaches Synaps how to generate behavior-tree plans for the resident VM agent.

**Acceptance criteria:**

- [x] `skills/vm-agent/SKILL.md` documents plan DSL v0.
- [x] Skill includes examples for sequence, selector, retry, wait_until, blackboard references, and traces.
- [x] Skill instructs the LLM to submit multi-step plans when state transitions are predictable.
- [x] Skill instructs the LLM to observe/escalate only at decision boundaries or after structured failure.

**Verification:**

- [x] Manual check: examples are valid JSON.
- [x] Manual check: examples correspond to implemented operation names.

**Dependencies:** Task 7  
**Files likely touched:** `synaps-skills/synaps-vm-automation/skills/vm-agent/SKILL.md`  
**Scope:** S

---

## Task 14: Create Windows UIA skill

**Description:** Add a Windows-specific skill that explains selectors, UIA concepts, AHK bridge behavior, and recovery strategy.

**Acceptance criteria:**

- [x] `skills/windows-uia/SKILL.md` documents role/name/automation-id selectors.
- [x] Skill defines selector cascade strategy: exact UIA, fuzzy UIA, semantic text, OCR/screenshot fallback, LLM escalation.
- [x] Skill includes Notepad and installer-wizard examples.
- [x] Skill warns against raw coordinates except as last resort.

**Verification:**

- [x] Manual check: examples are valid JSON.
- [x] Manual check: skill aligns with Windows backend capabilities.

**Dependencies:** Task 10  
**Files likely touched:** `synaps-skills/synaps-vm-automation/skills/windows-uia/SKILL.md`  
**Scope:** S

---

## Task 15: Add VM memory helper modeled after web skill

**Description:** Add memory scripts and conventions for recall/commit of VM automation lessons, selectors, plans, and failures.

**Acceptance criteria:**

- [x] Memory root is documented as `~/.synaps-cli/memory/vm/`.
- [x] `memory.py recall <query>` wraps `velocirag search` against the VM db.
- [x] `memory.py commit --kind selector|plan|failure|lesson` writes notes/traces and indexes them.
- [x] Failure records include task, error code, active window summary, visible controls, and lesson candidate.
- [x] Skills instruct the LLM to recall before acting and commit after success/failure.

**Verification:**

- [x] Manual check: recall handles missing VelociRAG gracefully.
- [x] Manual check: commit creates a note and indexes it when VelociRAG is installed.
- [x] Manual check: skill docs reference the memory workflow.

**Dependencies:** Tasks 12, 13, 14  
**Files likely touched:** `synaps-skills/synaps-vm-automation/scripts/memory.py`, `skills/*/SKILL.md`, `synaps-skills/synaps-vm-automation/README.md`  
**Scope:** M

---

## Checkpoint: After Tasks 11-15

- [x] Host plugin can control a libvirt VM.
- [x] Synaps skills explain safe usage and predictive planning.
- [x] VM memory workflow exists and mirrors web skill conventions.
- [x] Human reviews command safety and skill prompting before installer automation primitives.

---

## Task 16: Add installer automation primitive

**Description:** Implement a high-level `installer.advance` operation for common Windows setup wizards using UIA selectors and deterministic recovery.

**Acceptance criteria:**

- [x] Operation handles common buttons: Next, Continue, Install, Finish, Close.
- [x] Operation can optionally accept license checkboxes when configured.
- [x] Operation avoids Cancel/Back/Decline by default.
- [x] Operation stops and reports structured error dialogs instead of clicking blindly.
- [x] Operation records which selectors/buttons were used.

**Verification:**

- [x] Manual Windows VM check with a harmless installer or mock wizard.
- [x] Manual check: operation does not accept license unless `accept_licenses=true`.
- [x] Manual check: error dialog is captured in trace.

**Dependencies:** Task 10  
**Files likely touched:** `ops/ui.py`, `backends/windows_ahk.py`, `plans/executor.py`, `README.md`, `skills/windows-uia/SKILL.md`  
**Scope:** M

---

## Task 17: Add common popup/dialog recovery primitive

**Description:** Implement deterministic helpers for dismissing or reporting common dialogs.

**Acceptance criteria:**

- [x] `ui.dismiss_common_popups` can click configured safe buttons such as OK/Yes/Allow/Continue.
- [x] Avoid list prevents clicking Cancel/No/Decline unless explicitly allowed.
- [x] Operation reports external obstructing windows distinctly from app-owned modals.
- [x] Trace includes dialog title, process, selected button, and reason.

**Verification:**

- [x] Manual Windows VM check with simple message boxes.
- [x] Manual check: avoid-list buttons are not clicked by default.

**Dependencies:** Task 10  
**Files likely touched:** `ops/ui.py`, `backends/windows_ahk.py`, `skills/windows-uia/SKILL.md`  
**Scope:** S

---

## Task 18: Add Linux basic backend

**Description:** Add minimal Unix/Linux compatibility backend so the agent runs cleanly outside Windows and can grow AT-SPI support later.

**Acceptance criteria:**

- [x] Linux backend reports process/fs capabilities and UI capability gaps.
- [x] If `pyatspi` or `dogtail` is available, `/capabilities` reports optional accessibility support.
- [x] Unsupported UI operations fail with `CAPABILITY_UNAVAILABLE`.
- [x] Linux install script can install/run the agent as a user systemd service.

**Verification:**

- [x] Manual Linux check: server starts, health works, process/fs plan runs.
- [x] Manual Linux check: unsupported UI operation returns structured unavailable error.

**Dependencies:** Task 8  
**Files likely touched:** `backends/linux_basic.py`, `scripts/install-agent.sh`, `README.md`  
**Scope:** M

---

## Task 19: Add Windows install/service script

**Description:** Add a Windows setup script for the guest agent and its AHK dependencies.

**Acceptance criteria:**

- [x] `install-agent.ps1` installs Python package dependencies or documents expected venv.
- [x] Script validates AutoHotkey v2 availability.
- [x] Script creates config with token and localhost/default bind.
- [x] Token is generated locally, not printed by default, and stored in the current user's profile or a protected app-data path.
- [x] Config/token ACLs are restricted to the service user and Administrators.
- [x] Script can install or describe installing the agent as a Windows service/scheduled task using the least-privileged service account feasible for UI automation.
- [x] Firewall behavior is explicit: default localhost-only bind creates no inbound rule; non-local bind requires an intentional firewall rule and warning.
- [x] Token rotation instructions are documented and included in post-install help.
- [x] Script prints post-install verification commands that do not leak the token.

**Verification:**

- [x] Manual Windows VM check: run install script in dry-run/dev mode.
- [x] Manual Windows VM check: config file ACLs restrict non-admin/non-service-user reads.
- [x] Manual Windows VM check: token-authenticated `GET /health` works after install.
- [x] Manual Windows VM check: unauthenticated request fails.

**Dependencies:** Tasks 3, 10  
**Files likely touched:** `synaps-skills/synaps-vm-automation/scripts/install-agent.ps1`, `synaps-vm-agent/README.md`  
**Scope:** M

---

## Task 20: End-to-end Windows VM smoke test document

**Description:** Document a manual e2e smoke test from host virsh control through guest UI automation and memory commit.

**Acceptance criteria:**

- [x] Smoke test starts from a known clean snapshot when available, or records that no baseline snapshot exists.
- [x] Smoke test starts or verifies VM state with `vmctl.py`.
- [x] Smoke test verifies QEMU guest agent ping and guest agent `/health`.
- [x] Smoke test creates a snapshot.
- [x] Smoke test verifies network isolation: agent is not reachable from unintended interfaces/hosts under default config.
- [x] Smoke test verifies unauthorized request fails and token-authenticated request succeeds.
- [x] Smoke test runs Notepad plan through `/plan/run`.
- [x] Smoke test collects trace and commits a memory note.
- [x] Smoke test includes cleanup/revert instructions.

**Verification:**

- [x] Manual dry-read: commands are ordered and copy-pasteable.
- [x] Manual live run on Windows VM completes or records known blockers.
- [x] Manual security check results are captured in the smoke-test notes.

**Dependencies:** Tasks 11, 15, 19  
**Files likely touched:** `synaps-skills/synaps-vm-automation/README.md`, `synaps-vm-agent/README.md`, `docs/plans/2026-04-30-vm-automation-agent.md` if updating known blockers  
**Scope:** S

---

## Final checkpoint

- [x] `cd synaps-vm-agent && python -m pytest` passes.
- [x] Agent can run process/fs plans on Linux or Windows.
- [x] Agent can run Notepad UIA plan on Windows VM.
- [x] Host `vmctl.py status <vm>` reports VM state, IP, display URI, QEMU guest-agent health, and Synaps VM agent health.
- [x] Synaps plugin skills are present and examples are valid.
- [x] Memory recall/commit workflow works or fails gracefully when VelociRAG is missing.
- [x] Security defaults are documented and verified, including unauthorized rejection and intended-interface reachability only.
- [x] Cross-repo compatibility is verified: plugin reports required agent version, agent reports actual version, and mismatches fail clearly.
- [x] Human reviews before any broader autonomous GUI automation use.

---

## Risks and mitigations

| Risk | Mitigation |
|---|---|
| Guest agent exposes command execution | Token auth, localhost bind by default, allowlist, `allow_exec=false` option, audit logs, timeouts |
| UIA selectors are brittle | selector cascades, semantic names, blackboard, traces, memory learning |
| AHK subprocess overhead | acceptable for v0; later replace with persistent AHK socket bridge |
| Windows privilege/UAC boundaries | detect access denied and report; avoid UAC automation by default |
| Linux UI automation fragmentation | start with capability reporting and process/fs; add AT-SPI later |
| LLM over-observes and acts slowly | skill instructions require predictive batch plans and local waits |
| Plan DSL grows too quickly | keep v0 small; add high-level primitives only after trace evidence |

---

## Implementation discipline

Before coding starts:

- [x] Human approves this plan and convergence mode.
- [x] Create a dedicated git worktree for implementation.
- [x] Use TDD where feasible, especially executor/security/process modules.
- [x] Keep each task in a separate commit unless tightly coupled.
- [x] Do not implement on the primary checkout.

Suggested worktree command:

```bash
git fetch origin --prune
git worktree add -b feat/vm-automation-agent ../.worktrees/SynapsCLI-vm-automation-agent origin/main
cd ../.worktrees/SynapsCLI-vm-automation-agent
```
