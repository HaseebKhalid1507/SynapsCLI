# C-product — "better than any other agent runtime": #112 × daemon product scope

> **Read-only recon. No code edits, no cargo, no git writes.**
> Claims marked **(web)** are web-sourced and not verified from local code.
> Everything else is grounded in local repo reads with file:line citations.

---

## Q1 — Durable context: how does each runtime handle context exhaustion?

### Landscape

| Runtime | Strategy | Mechanism | Cross-session recall | Local code read? |
|---|---|---|---|---|
| **Synaps #112** | Rollover + archive + Axel retrieval | At soft pressure (e.g. 250k/1M), prepare a smaller context keeping all real user turns, seal eligible source to a private archive, get a durable-head checkpoint from the frontend, then continue in a fresh window with same session/env/permissions. `memory_search(source="history")` + `memory_fetch` for retrieval. | Yes via shared Axel brain with repo-scoped project archives; forum posts visible to all worktree sessions | ✓ `jr-112/docs/specs/context-continuation.md:1-340` |
| **jcode 0.76** | Summarisation compaction (reactive/proactive/semantic) | At 80% of budget, background task summarises old turns into a `[COMPACTED]` block; emergency hard-compact at 95% drops messages. 10 recent turns kept verbatim. No rollover, no archive, no retrieval of compacted content. | No — summary replaces history; originals gone from model context. Per-session only. | ✓ `jcode-compaction-core/src/lib.rs:1-60`, `jcode-base/src/compaction.rs:1-80` |
| **Claude Code** | **(web)** In-context compaction — summarises the conversation when nearing the limit; auto-compact flag. No rollover or external memory. | **(web)** Summarisation; no archive; `/compact` manual. | **(web)** No cross-session; CLAUDE.md as the only persistence mechanism. | ✗ |
| **Codex CLI** | **(web)** Single-shot `run_single`; context is the session. No compaction. Limited by provider window. | **(web)** No management — relies on large windows (Codex model). | **(web)** No. | ✗ |
| **Goose** | **(web)** LLM-based summarisation when approaching limits. | **(web)** In-place summary compaction. | **(web)** No cross-session memory. | ✗ |
| **opencode** | **(web)** No compaction; session-scoped. | **(web)** Relies on large windows. | **(web)** No. | ✗ |
| **pi (coding-agent)** | Not observed in local code | Session-scoped messages; no compaction mechanism found in `packages/coding-agent/src/` | No | ✓ (absence confirmed) |

### What #112 does differently — and honestly

**Unique combination:**
1. **Rollover, not summary.** Keeps real user turns verbatim; no LLM-generated summary replaces your words. The old context is sealed to a private archive and retrievable by exact ID (`memory_search(source="history")` → `memory_fetch`). This means the model can re-read the *actual* tool output from turn 47, not a summary's interpretation of it.
   - `context-continuation.md:111-113` — "Prepare a smaller context, preserving actual user-authored turns (including image-only turns), system/developer messages, the last assistant/result tail, and all tool-call/result dependencies of retained messages."
2. **Axel-backed retrieval.** Archives live in the `.r8` database scoped to the project (`shared-axel-repositories.md:88-98`). The model fetches specific facts from prior windows on demand, not from a lossy summary baked into context.
3. **Project agent forum.** Subagents can post to a persistent project forum (`agent-forum.md:1-22`), so a compacted window's insights survive as structured durable notes, not ephemeral tool output.
4. **Unproductive rollover recovery.** If the candidate can't meaningfully shrink, it continues with the current context instead of erroring (`context-rollover-recovery.md:1-40`). jcode's compaction can triple-fire without reducing size because it undercounts images (`jcode-compaction-core/src/lib.rs:47-58`).

**Honest weak spot (JR's own words):**
- "No claim of a measured 2× end-to-end token reduction" — `context-continuation.md:249-250`: "No cost/quality improvement claim is inferred from that smoke." The live test used synthetic low thresholds, not a real 350k-token session.
- The archive index is "a bounded scan, not a large-corpus search engine" (`context-continuation.md:205-206`). Retrieval quality over many windows is unproven.
- "The prototype intentionally retains every real user turn; instruction-heavy histories may stop instead of shrinking" (`context-continuation.md:253-254`).

**What measurement would prove it:**
A controlled benchmark: N-window coding task (e.g., refactor a 50-file module), same task on Synaps rollover vs. jcode summarisation vs. Claude Code compact. Metrics: (a) task completion rate, (b) total tokens consumed, (c) retrieval accuracy on facts from window 1 at window 5. This does not exist yet.

**How daemon's multi-session view makes it better:**
- Shared Axel brain (`shared-axel-repositories.md:1-10`): two daemon sessions on the same repo share one `.r8` database. Session A's archived context is searchable by Session B, scoped by repository key.
- Forum cross-pollination: a subagent swarm writes findings to the forum; the coordinator session or a later human session reads them — even after the subagent's context is long gone.
- This is genuinely new. jcode's compaction summaries are per-session and die with the session. Claude Code's CLAUDE.md is file-level notes, not structured queryable history.

---

## Q2 — Autonomous driving: competitive comparison

### Landscape

| Runtime | Autonomous mode | Permission model | Survive terminal close? | Multi-model failover | Steerability | Local code? |
|---|---|---|---|---|---|---|
| **Synaps #112** | `/auto start [--turns N] [--for Xh] [--context auto\|off] -- <goal>` via external Python plugin. Permissioned `session.drive` grant. | Fail-closed. Explicit user command only. Plugin proposes; host validates model allowlist, bounds, deadline. All commands (incl status) revoke. | TUI-only today. Grant is session-RAM only — no persistence/resurrection. | Yes: ordered favorites list (4 models), automatic failover on auth/quota/rate-limit. 3 transient retries per favorite, 5-min cooldown on full wrap. | Bounded FIFO for human text steering mid-run. Draft edits don't revoke. | ✓ `autonomous-plugin.md`, `session-drivers.md` |
| **jcode overnight** | `/overnight start <duration> [mission]`. Coordinator session spawns headless swarm agents. Manifest, events JSONL, task cards, HTML review, morning report. | No tool gates at all (`RECON-jcode.md` §2.3: static risk classifier, no interactive prompt). Catastrophic commands hard-denied. | Yes — headless sessions survive disconnect (`server/headless.rs:37-80`). But *interactive* sessions abort on disconnect (`client_disconnect_cleanup.rs:253`). | Not observed — single provider per coordinator session. | Not observed — no mid-run steering; cancel only. | ✓ `jcode-overnight-core/src/lib.rs`, `jcode-app-core/src/overnight.rs` |
| **Claude Code** | **(web)** `--dangerously-skip-permissions` + max-turns loop. `claude -p "goal" --max-turns 100 --dangerously-skip-permissions`. | **(web)** All-or-nothing: either every tool needs approval, or `--dangerously-skip-permissions` blanket-allows everything. No per-tool or per-session policy. | **(web)** No — runs in-process. | **(web)** No — single provider. | **(web)** No mid-run steering. | ✗ |
| **Codex CLI** | **(web)** `--full-auto` mode with sandbox. | **(web)** Network-disabled sandbox; all file writes allowed within sandbox. | **(web)** No daemon; single-shot. | **(web)** No. | **(web)** No. | ✗ |
| **Goose** | **(web)** Session mode with `--non-interactive`; MCP-based tool approval. | **(web)** Per-tool MCP approval; configurable allow/deny lists. | **(web)** No daemon. | **(web)** Provider configurable but no auto-failover. | **(web)** No mid-run steering. | ✗ |
| **OpenHands/Devin** | **(web)** Cloud-based; always autonomous. Sandbox container. | **(web)** Container isolation; user approves the plan, not individual tools. | **(web)** Cloud-native: yes. | **(web)** Yes — cloud infrastructure. | **(web)** Chat-based steering. | ✗ |

### What's unique in #112's driver

1. **Plugin-defined policy, host-enforced boundaries.** The Python plugin owns loop logic, prompt text, retry strategy, and favorites. The Rust host owns the immutable allowlist, monotonic deadline, generation checks, and lifecycle invalidation. No built-in policy in Rust — upgradeable without recompiling. (`autonomous-plugin.md:1-8`)
2. **Feedback fingerprinting.** Host compares completed output against a 4-turn ring buffer and reports `changed|repeated|empty|unknown` to the plugin. Three consecutive `repeated`/`empty` → automatic favorite advance. This is semantic stall detection without reading transcripts. (`autonomous-plugin.md:134-150`, `session-drivers.md:161-186`)
3. **Wall-clock continuation.** `time_checkpoint_version: 1` lets the driver continue after wall-clock exhaustion on the same model/effort, rather than stopping or falling back. Works with and without `/context auto`. (`autonomous-plugin.md:105-130`)
4. **Human steering without revocation.** Submitted text mid-run enters a bounded FIFO, delivered as separate user messages on the next turn. Draft edits never revoke. This is "pair programming with an autonomous agent" — nobody else has this. (`session-drivers.md:66-108`)

### The actor-native version — what daemon enables

| Capability | Status | "Nobody has this"? | Notes |
|---|---|---|---|
| **Runs survive terminal close** | NOT YET — driver is TUI-only (`autonomous-plugin.md:7`: "local TUI sessions only"). Moving driver → actor is the goal. | ✓ if done. jcode overnight headless survives, but interactive doesn't. Claude Code/Codex/Goose don't. | The play. |
| **Steerable from any client** (`synaps send`) | Partially works — `send` delivers messages to daemon sessions today (`soak note F-series verified`). But driver grant is TUI-bound — `send` can't steer a driven session yet. | ✓ if driver moves to actor. Phone `attach --observe` + `send` from another terminal = remote steering. | Requires driver grant to live on the actor, not the TUI. |
| **Observable from phone** (`attach --observe`) | Works today for non-driven sessions. | Table stakes for daemon runtimes, but no CLI agent has it yet. | |
| **Resumable across daemon reload** | JR says "never restore a run" — grant lives in process memory only, no persistence. Reload checkpoints abort context. | Both sides have merit. FOR: long overnight run survives an upgrade. AGAINST: restoring a stale grant with potentially changed env/tools/permissions is dangerous; a driver that was mid-turn when killed has unknown partial side effects. **Recommendation: don't restore. A reloaded session with abort context offers `/auto start` again; the human decides.** | `autonomous-plugin.md:66`: "Process restart must never restore an active run automatically." |
| **N concurrent autonomous sessions** | Daemon already runs N sessions (`soak note: 10 simultaneous adopts → 10 sessions`). Each could have its own driver grant. | ✓ genuinely new. jcode overnight runs one coordinator + N headless workers but they're not independently steerable autonomous sessions. | Shared sidecar set means N sessions don't spawn N×M MCP processes (`daemon-mode.md: sidecars per daemon, not per session`). |

### Safety: new failure modes with no human at the terminal

| Risk | Severity | Concrete failure mode | Mitigation |
|---|---|---|---|
| **Spend caps** | HIGH | N autonomous sessions × unlimited turns × 4 model failover = unbounded spend. No per-daemon or per-session cost cap exists today. | MUST ADD: per-session cost ceiling in the grant (`max_cost_usd`); per-daemon aggregate ceiling; hard stop on breach, not just a notice. |
| **Tool approval with no owner** | HIGH | A driven session parks (no clients, 60s grace). The model requests a destructive tool. No client to answer the prompt → approval hangs → driver timeout → blocked → stop. Correct fail-closed behavior, but: (a) the session is now stuck, (b) no notification to any device. | Actor-side driver should hold a "last known notification channel" — push to the phone via `attach --observe` push / email / webhook. Or: driven sessions don't park (`keep-warm` auto-set). |
| **Driver-armed session that parks** | MED | Grant is TUI-RAM only today, so park = grant lost = safe. But if grant moves to actor: a parked session with a live grant unparks when someone attaches, and the old grant might fire with a new client that didn't authorize it. | Grant must be invalidated on park. Unpark = re-consent required. |
| **Wrong env** (F25) | **CRITICAL for autonomous** | An autonomous run in the wrong env is worse than an interactive one: the human might notice `python` resolving to system python; the autonomous agent won't. It will happily `pip install` into the wrong venv, run tests against the wrong DB, etc. | Session-identity plan (T1-T3) MUST land before driver→actor. Non-negotiable. |
| **Orphan tool children** (F8) | MED | Autonomous session spawns a build, daemon crashes, build runs to completion with no supervision. N sessions → N orphans. | PDEATHSIG / process group — tracked as F8 in soak, out of scope for #112 but amplified by autonomous. |

---

## Q3 — Attachments: image/PDF/file across the wire

### Comparison

| Runtime | Image | PDF | Text file | Clipboard paste | Over daemon wire | Size limit | Local code? |
|---|---|---|---|---|---|---|---|
| **Synaps #112** | ✓ base64 `image/source` (Anthropic native, OpenAI `image_url` data URI / Responses `input_image`). Revalidate on model switch. | ✓ `document/source base64 application/pdf` (Anthropic native; Codex: `input_file`; Astra: rejected; spark: rejected). | ✓ UTF-8 as `document/source text text/plain` (OpenAI Responses: `input_text`). | ✗ Explicitly excluded: "No clipboard image decoding" (`multimodal-attachments.md:6`). | Preserved in session JSONL (inline bytes, no external file reference). Archive EXCLUDES media source bytes — metadata only. | Content-based MIME + decoded size bounds; frame cap 64 MiB. | ✓ `jr-112/docs/specs/multimodal-attachments.md`, `jr-112/docs/multimodal.md` |
| **pi (coding-agent)** | ✓ `@file` with auto-resize to 2000×2000. `image/png,jpeg,webp,gif`. | Not observed in attachment pipeline. | ✓ `@file` for text. | ✓ `clipboard-image.ts` — Wayland/X11/macOS clipboard read, photon-based processing. | N/A — no daemon. | `resizeImage()` with max dimension; 50 MB `DEFAULT_MAX_BUFFER_BYTES` for clipboard. | ✓ `pi-mono/packages/coding-agent/src/cli/file-processor.ts`, `utils/clipboard-image.ts` |
| **jcode 0.76** | ✓ `jcode-terminal-image` crate; side-panel push (`side_pane_images`/`generated_image` base64). | ✓ `jcode-pdf` crate (extraction). | ✓ via tool reads. | Not observed in local code. | Base64 in `ServerEvent`; no frame cap on main socket (unbounded `read_line` — `RECON-jcode.md` §7). | jcode compaction emergency: `EMERGENCY_IMAGE_MAX_CHARS = 1024` strips images at 95% context. | ✓ `RECON-jcode.md` §8 |
| **Claude Code** | **(web)** ✓ images in prompt. | **(web)** Not directly; use `Read` tool. | **(web)** Via `Read` tool. | **(web)** Not observed. | **(web)** N/A — no daemon. | **(web)** Unknown. | ✗ |
| **Codex CLI** | **(web)** ✓ `input_image` in Responses API. | **(web)** `input_file`. | **(web)** `input_text`. | **(web)** No. | **(web)** N/A — no daemon. | **(web)** Unknown. | ✗ |

### Where #112 stands; what's missing

**Strengths:**
- Provider-polymorphic serialisation: same attachment works across Anthropic native, OpenAI chat, OpenAI Responses, and Codex. Model+transport capability check with static evidence rows + runtime `input_modalities` (`multimodal-attachments.md:10`).
- Revalidation on model switch — resume a session with a different model and unsupported attachments become explicit errors, not silent drops.
- Session JSONL persists inline bytes so resume needs no original files.

**Missing:**
1. **Clipboard paste** — pi has it (`clipboard-image.ts`). A screenshot-paste workflow is table stakes for debugging visual issues. Not in #112.
2. **Drag-and-drop** — no runtime has this in terminal; it's a GUI concern. Not a gap for CLI.
3. **Size limits over daemon frame cap** — frame cap is 64 MiB (`daemon-mode.md:137`). A 20 MB PDF base64-encodes to ~27 MB — fits. But a few attachments in one message could exceed 64 MiB. The frame cap is enforced symmetrically; oversize → `Error` + close (`daemon-mode.md:137-141`). **Risk:** an `Attached` replay of a session with many attachments could exceed 64 MiB. Digest mode (`SYNAPS_CLIENT_HISTORY=digest`) mitigates — `api_messages = []` in `Attached` — but full-mode clients would fail.
4. **Archive excludes media** — context rollover loses image bytes; only metadata preserved. A model can't re-examine a screenshot from 3 windows ago. Correct for storage (16 MiB archive budget), but reduces retrieval quality on visual sessions.

---

## Q4 — The daemon as a differentiator

### What we can claim honestly (measured numbers)

| Claim | Evidence | Source |
|---|---|---|
| **2.5 MB thin clients** | RssAnon 2.27 MB idle post-purge (digest mode, THP disabled) | `memory-budget.md` Phase 4 table: "G1: 2.27 MB" |
| **3.2 MB marginal per session** | Daemon anon 0.91 MB + client 2.27 MB = 3.18 MB | `memory-budget.md` Phase 4: "G5: 3.18 MB" |
| **10+ concurrent sessions, 36 MB daemon** | "10 simultaneous adopts → 10 sessions, clients 2.5 MB each, daemon 36 MB anon @ 11 live" | `soak note: line "What works"` |
| **Attach in 7-10 ms** | First frame in 7-10 ms (digest mode) | `memory-budget.md` Phase 4: "G4: 7-10 ms" |
| **Park/unpark in 2 ms** | "reattach unparks in 2 ms; model has full history" | `soak note: "Park after 60 s grace"` |
| **Sidecars shared, not per-session** | "procs/session == 1 (the attach client) and daemon procs constant (3)" | `daemon-mode.md` §Memory acceptance table |
| **Survive daemon SIGKILL** | "SIGKILL → plain synaps reaps stale and boots in-process" | `soak note: verified` |
| **In-place reload** | "same pid, generation 1→2, flock held, conversation identical after reconnect" | `daemon-mode.md` §Reload |
| **Scrollback-capped: no unbounded growth** | "slope ≤ 1.5 MB over turns 30→80" with 2 MiB scrollback cap | `memory-budget.md`: "G6" |

### Competitor comparison

| Feature | Synaps daemon | jcode 0.76 | Claude Code | Codex CLI | Goose | opencode |
|---|---|---|---|---|---|---|
| Long-lived daemon | ✓ | ✓ | ✗ | ✗ | ✗ | ✗ |
| N concurrent sessions | ✓ (tested 10+) | ✓ | ✗ | ✗ | ✗ | ✗ |
| Turn survives terminal close | ✓ (detach keeps running) | ✗ (abort on disconnect, `RECON-jcode.md` §3.4) | ✗ | ✗ | ✗ | ✗ |
| Multi-client mirroring | ✓ (mirror/observe/takeover) | ✗ (one owner, takeover-only) | ✗ | ✗ | ✗ | ✗ |
| Park/evict idle sessions | ✓ (60s grace, Runtime dropped, journal restored) | ✓ (evict on disconnect, load on resume) | N/A | N/A | N/A | N/A |
| Permission prompts over wire | ✓ (`Prompt{Confirm}` to owning client) | ✗ (no prompts at all) | N/A | N/A | N/A | N/A |
| Shared MCP/sidecars | ✓ (per daemon) | ✓ (SharedMcpPool) | N/A | N/A | N/A | N/A |
| In-place reload (same PID, same flock) | ✓ | ✓ (exec-in-place) | N/A | N/A | N/A | N/A |
| Thin client (< 5 MB) | ✓ (2.3-2.5 MB) | ✗ (full binary = client) | N/A | N/A | N/A | N/A |
| Digest mode (O(tail) wire cost) | ✓ | ✗ (one JSON per token, full history on attach) | N/A | N/A | N/A | N/A |

### Where we're still behind

| Gap | Who has it | Severity | Notes |
|---|---|---|---|
| **Windows** | jcode has named-pipe transport (`jcode-transport/src/windows.rs`, 471 lines). | MED | Our daemon is Unix-only; `SYNAPS_DAEMON=0` falls back to in-process. |
| **Remote daemon over TCP/SSH** | jcode has `--socket PATH` + `--remote-working-dir` + WS gateway. | LOW (v0) | Our spec explicitly defers remote (`synaps-daemon-protocol-v0-spec.md` §3 non-goals). Socket forwarding over SSH works today but is undocumented. |
| **Multi-user** | Nobody has real multi-user in local CLI agents. | LOW | Same-UID trust model is correct for v0. |
| **Session env propagation** (F25) | jcode partially has `terminal_env` in Subscribe (but only for window routing, not tool exec — `RECON-jcode.md` §3.1). | **HIGH** | Our session-identity plan (T1-T3) is designed but unbuilt. This is the #1 gap before autonomous-in-daemon. |
| **Clipboard image paste** | pi has it. | MED | See Q3. |
| **Idle exit / session GC** | jcode exits after 5 min with zero clients. | LOW | We have `--idle-exit` but no default; parked sessions accumulate (`soak note F16`). |

---

## Q5 — Prioritised recommendation

### The question

Given finite Jawz + Haseeb time, which of these has the highest product leverage per day?

1. Engine half of #112 now (context-continuation, archive, forum, multimodal, driver types — pure engine crate, no TUI conflicts)
2. Driver → actor (move TUI session_driver into SessionActor so autonomous runs survive terminal close)
3. Session-identity plan (T1-T7: env propagation, --system by content, journal env, extension protocol)
4. F19 (empty-response data loss — model says text + tool_use, end_turn with `content: []`, actor drops the whole turn)

### The answer: **Session-identity first. Then engine half. Then F19. Driver→actor last.**

**Why session-identity is #1 (T1-T3: 3 days, M):**

Every other feature is built on a lie without it. The daemon today runs tools in the daemon's env — wrong PATH, wrong VIRTUAL_ENV, wrong AWS_PROFILE, no SSH_AUTH_SOCK (`soak note F25`, verified on bella). This means:
- Every daemon session is already subtly broken for anyone who uses virtualenvs, nvm, conda, direnv, or cloud credentials (i.e., every developer).
- Shipping autonomous driving on top of broken env is a **force multiplier for damage** — an autonomous agent silently installing packages into the wrong Python or deploying to the wrong AWS account is worse than a human doing it interactively, because the human notices.
- Session-identity (T1-T3) is a 3-day sprint with a clear acceptance test (`session_identity_differential.rs` proves it or fails). The risk is bounded. The payoff is: every existing daemon feature actually works correctly.

**Why engine half is #2 (5-7 days, L):**

This is the high-leverage merge. The 20 conflict files split roughly: engine-side (10 files in `agent-engine/`) and TUI-side (10 files in `agent-tui/` + `src/`). COMMON.md establishes that the engine half has +1 conflict vs soak-fixes (trivial `agent-core/src/core/mod.rs`). The engine half gives us:
- Context-continuation + archive + rollover recovery (the core differentiator from Q1)
- Forum protocol and Axel backend (cross-session memory)
- Multimodal attachment types + serialisation + capability checks
- `session_driver.rs` engine types (Grant, Proposal, Selection, Outcome, `prepare()`) — the foundation for actor-side driving without the TUI wiring
- Shared Axel repository identity

This is all pure policy/data code with no TUI dependency. It can land on `soak-fixes` without touching the 10 TUI conflict files, giving us the entire feature surface for testing and daemon integration. The TUI integration is a separate, later merge.

**Why F19 is #3 (1-2 days, S):**

F19 is data loss (`soak note F19`): "Empty model response on round ≥2 is treated as the #130 empty_response error → actor drops the whole turn (text + tool_use + tool_result), journal never written." This hits 3/3 repro with sonnet-4-6. It's pre-existing and model-ordering dependent but it erodes trust in the daemon. One day, surgical fix in `runtime/stream.rs:721`. Ship confidence before shipping features.

**Why driver→actor is last (5-8 days, L):**

Moving the driver into the actor is the flashiest feature ("close your laptop, autonomous work continues"). But it depends on *all three items above*:
- Without session-identity, the autonomous run uses the wrong env (catastrophic).
- Without the engine half, the driver types (Grant, Proposal, Selection) don't exist on the engine side.
- Without F19, an autonomous run can silently lose turns, and no human is watching to notice.

Additionally, the TUI session_driver.rs (1811 lines) is deeply entangled with App state, timers, draft preservation, and the steering FIFO. Extracting it into the actor is a redesign, not a port. The actor needs: a timer task (delay between turns), a notification mechanism (no client → what happens to approval prompts?), lifecycle invalidation without TUI event loop, and integration with park/unpark semantics. This is a week of work that goes smoothly only after the foundations are solid.

### Risks of shipping driver actor-side before session-identity

| Risk | Likelihood | Impact | Why |
|---|---|---|---|
| Autonomous agent uses wrong Python | CERTAIN until T2 | HIGH — wrong packages installed, wrong tests run, wrong artifacts built | F25 verified: daemon bash sees daemon env |
| Autonomous agent uses wrong AWS credentials | HIGH for cloud devs | CRITICAL — wrong account, wrong region, cost/security incident | Same mechanism as F25 |
| `--system ./prompt.md` silently ignored | CERTAIN until T4 | MED — agent runs with default identity, subtle behavior changes | F26 verified |
| Agent runs after daemon reload with stale env | HIGH until T5 | MED — env snapshot lost on park if not journaled | T5 is the journal-env task |
| Shared sidecar sees daemon env, not session env | CERTAIN until T6 | MED — web-tools/Axel process spawns in wrong context | T6 is the extension protocol task |

**Bottom line:** Session-identity is not a nice-to-have prerequisite for autonomous driving in the daemon. It is a *hard dependency*. Shipping driver→actor without it is shipping a gun that aims at the user's foot. The sequence is: **T1-T3 → engine half merge → F19 → driver→actor**.

### Size estimates

| Work item | Size | Hours | Dependencies |
|---|---|---|---|
| Session-identity T1-T3 (env wire + apply + differential test) | M | 16-24 | None — lands on soak-fixes |
| Engine half of #112 (10 engine-side files, cherry-pick/rebase) | L | 32-48 | None — parallel with T1-T3 if different implementer |
| F19 empty-response fix | S | 6-10 | None |
| Session-identity T4-T5 (--system by content, journal env) | S+S | 8-12 | T1-T3 |
| Driver → actor | L | 40-56 | Engine half + T1-T3 + F19 |
| Session-identity T6-T7 (extension protocol + built-in opt-in) | M | 16-24 | T1-T3, JR spec review |

### Open decisions for Haseeb

1. **Full env vs allowlist for T1?** Plan says full with denylist for secrets. Confirm — this is the right call (allowlists are lists of things you forgot).
2. **Should `*_API_KEY` in client env ever become session env?** Plan leans stripped+ignored (broker owns creds). Agree — agent tools shouldn't need raw API keys; the broker handles auth.
3. **F19 fix strategy:** preserve the text + tool_use + tool_result even when end_turn has `content: []`, or treat zero-content end_turn as a valid response (the text was already streamed)?
4. **Engine half merge strategy:** cherry-pick engine files onto soak-fixes, or create a new branch from soak-fixes and replay just the engine-side hunks from #112? The latter is safer (hunk-aware, S235-compliant) but slower.
5. **Autonomous cost cap:** before driver→actor ships, we need a per-session and per-daemon cost ceiling. What's the default? Suggestion: $10/session, $50/daemon, configurable via grant and daemon config.
