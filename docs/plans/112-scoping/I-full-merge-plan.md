# I — Full #112 landing plan (everything not yet on dev)

**Status:** plan · **Base:** `dev` @ 816c143d (engine half landed via #120/#121; adopt/idle/F18/evict via #122) · **Inputs:** F (attachments), G (context UX + sidecar + residual TUI), H (engine residual audit), E (driver→actor spec), SYNTHESIS decisions 1–6, PROCEDURE lessons.

## What "entire #112 on dev" means after #120

Five features. Engine machinery for all five is in. What is not:

| Feature | Missing on dev | Doc | Hours |
|---|---|---|---|
| Durable context | TUI never renders `ResponseStart/Reset`; no blocked-head UX; `/context` + `/budget` already route via `EngineCommand` (G Q3 resolved) | G1, G6 | 4 |
| Axel backend | sidecar UI: non-blocking startup, Loading pill, self-config (`tui/sidecar.rs` 511→1235) | G2, G3 | 5 |
| Project forum | nothing — usable via config | — | 0 |
| Multimodal attachments | **all user surfaces**: `/attach`, drafts, submit-with-blocks, chat, rpc; `Submit.attachments` exists but the actor ignores it (`..`) | F1–F6 | 13–16 |
| Autonomous plugin | **Wall 2**: TUI host loop → actor commands/events; 3 gates | E P0–P10 | 37–55 |
| Engine residuals | **4 LOST body-level hunks in stream.rs** (H), 25+14+18 portable tests, `AGENTS.md`, `help.json`, docs | H §4 | 8 |
| Small TUI | `/` command whitespace bug, `apply_interactive_command_result`, lifecycle flush, contract.json reorder, 1 budget test | G4, G5, G7 | 3 |

**Total: 70–91 h.** Three independent tracks + one gated track.

## Why this is ordered the way it is

1. **H first, alone.** Four production bugs in the streaming path (pre-cancel guard, unstarted-tool ledger, model-aware tool-output validation, budget exhaustion message) plus ~57 upstream tests that will *detect* any further loss. This is the "make the base honest" phase — everything else stacks on it, and F3's validation needs LOST-3 in place.
2. **F, G in parallel** — disjoint files (F: input/commands/dispatch/app attachment paths, chat.rs, rpc.rs; G: stream_handler, transcript, sidecar.rs, draw.rs, help.json, docs). Both land as render/thin-client work on seams the actor already exposes; neither touches the driver.
3. **E last, gated.** Wall 2 needs answers to E §5 (3 questions) and D-security S1–S3 mechanisms (spec'd in E §3). P0–P2 (~6 h of plumbing: wire types, `feedback.rs` move, single-tenancy grant) can start any time; P3+ waits for the gates.

## Phases

### Track H — engine residuals (1 worker, ~8 h) → PR `fix/112-engine-residuals`
| # | Work | Files | Test | Gate |
|---|---|---|---|---|
| H1 | LOST-4: `finish_budget_exceeded!` uses `budget_meter.exhaustion_error(dimension)` | `runtime/stream.rs` | port `wall_clock_exhaustion_can_resume…` (tests/turn_budget_stream.rs) | `cargo test -p synaps-engine -- budget` |
| H2 | LOST-1 `await_provider_call` + LOST-2 `await_tool_call` (pre-cancel short-circuit; `started` flag → `interrupted_started` only for polled tools) | `runtime/stream.rs` | port the 4 upstream cancellation tests (`provider_pre_cancellation_never_polls…`, `…preserves_cooperative_partial_cleanup`, `…bounds_and_drops_uncooperative…`, `tool_pre_cancellation_never_polls…`) — they need `drive_with_backend*` helpers, port those too | `-- cancel` |
| H3 | LOST-3 `validated_tool_output` in the streaming path (both tool sites; keep F28 `errored` and `select_tool_result_content`) | `runtime/stream.rs` | port `rewritten_or_truncated_summary_drops_rich_blocks`, `unchanged_summary_retains_rich_array…`, `unsupported_or_malformed_media_becomes_explicit_text`, `plain_text_and_errors_keep_legacy_truncation` | `-- rich_output` |
| H4 | Port remaining test-only symbols: memory-backend config (5), forum/author (7, ctor adjust for `compose_system_prompt(_, forum)`), context continuation (6, StreamSession ctor adjust), `history_media_limit_rejects_before_lossy_pruning` | `runtime/{mod,stream}.rs`, `tools/subagent/mod.rs` | themselves | `cargo test -p synaps-engine` ≥ 2113 + ~57 |
| H5 | `AGENTS.md` (+36), `assets/help.json` (+21, regen `docs/tools.json` if it changes), `docs/extensions/contract.json` reorder, `docs/sidecar-protocol.md` | docs | drift tests | workspace |
| H6 | `symbol-audit.py` → 0 non-test production symbols missing except the documented deliberate list (E/F/G-owned + `await_*` now ported); append LEDGER row | — | — | — |

### Track F — attachments (1 worker after H, ~13–16 h) → PR `feat/112-attachments`
Wire decision (F §3.2, adopted): **client loads bytes and ships canonical content blocks** (`Vec<serde_json::Value>`); the daemon never touches client paths (same reason T4 resolves `--system` client-side). Actor validates structurally via `validate_attachment` + model gating via `validate_messages`, rejects with `SystemNotice`. Size: existing `HISTORY_IMAGE_BYTE_CAP` applies; per-frame cap = `DAEMON_MAX_FRAME_BYTES`.
| # | Work | Hours |
|---|---|---|
| F1 | `SessionCommand::Submit.attachments: Vec<Value>`; actor `submit()` stops ignoring it; serde round-trip + unit tests | 2 |
| F2 | TUI `/attach` `/attachments` `/detach`, `PendingAttachments` on App, Enter-path idle guards; port `attachment_only_enter_and_busy_draft_retention` | 2 |
| F3 | TUI submit builds blocks → wire; `Refused` restores editor + keeps drafts; display summaries; busy guards | 3 |
| F4 | headless `synaps chat` (actor mode): `/attach` family + blank-line attachment-only submit; piped fail-stop | 2 |
| F5 | rpc: `rpc_attachment_paths` + `load_rpc_user_content` (absolute, no `..`, `O_NOFOLLOW`, regular file), `attachments.disclosure` event; port `attachment_tests` (#395) | 3 |
| F6 | `docs/multimodal.md` accuracy pass, symbol audit, LEDGER | 1 |
DARK: no behaviour change unless the user runs `/attach` or sends `attachments:[…]` over rpc.

### Track G — context UX, sidecar UI, residual TUI (1 worker, parallel with F, ~12 h) → PR `feat/112-tui-context-sidecar`
| # | Work | Hours |
|---|---|---|
| G1 | `ResponseStart/ResponseReset` render arms (`stream_handler.rs:192` no-op → real) + `TranscriptStore::reset_response_preview`; orphaned-reset guard (G Q2: ignore silently) | 2 |
| G2 | `SidecarUiStatus::Loading` + draw pill | 1 |
| G3 | sidecar non-blocking startup (`SidecarStartup`, `toggle/next_startup/finish_startup/drain_events/retain_enabled/status`), in-process path only; socket path no-op; port `async_startup` tests (unix) | 4 |
| G4 | `help.json` `/budget`, `AGENTS.md` (if not done in H5) | 1 |
| G5 | `docs/sidecar-protocol.md`, contract.json (if not done in H5) | 1 |
| G6 | `tests/turn_budget_stream.rs` new test (if not done in H1) | 2 |
| G7 | `/` command `splitn(2, char::is_whitespace)` fix (`input.rs:476`), `apply_interactive_command_result` | 1 |
Blocked-head UX: dev's actor already emits `SystemNotice` on a latched head (`actor.rs` submit guard); verify wording vs upstream, no new wire.
DARK: render code only fires on events the engine only sends with `context_management.mode = auto`; sidecar Loading only when a plugin negotiates `ready_after_init`.

### Track E — Wall 2, driver→actor (E-driver-port-spec.md; 37–55 h) → PR `feat/112-driver-actor`
Prereqs before P3: answers to E §5 (feedback.rs crate placement; `session_id` in `PollRequest` now vs later; `max_cost_usd` plugin-proposed vs host-imposed). P0–P2 (~6 h) can run alongside F/G.
Then P3 (DriverState + arm/revoke), P4 (`driver_tick()` — the risk, 8–12 h, own review), P5–P7 (stream hooks, submit routing, S1/S3/S9 gates as code + 12 tests), P8 (TUI thinning 1811→~100 lines), P9 (attach/send clients), P10 (docs). Checkpoints after P4 and P7.

## Sidecar ownership under the daemon (G Q1) — decision needed, not blocking G
Upstream: `App` owns `SidecarManager` (per-TUI). Dev daemon: sidecars would be daemon-side, N sessions → per-session or per-daemon managers? G3 lands the in-process path only and leaves the socket path a no-op; the daemon-side design is a follow-up (ties to E's single-tenancy grant). Recommend per-session (matches `MemoryBinding` per-session scoping, §4).

## Method (non-negotiable, from PROCEDURE.md lessons)
- One worker per track, sequential phases inside a track, `symbol-audit.py` after every phase **plus** a hunk read of every `+` block in touched files (H found 4 body-level losses the symbol audit cannot see).
- Every hunk becomes either an actor-side command/event or a renderer. Nothing calls `runtime.*` from `agent-tui`.
- Bella-only builds. Workspace gate + CI-shaped clippy (`--all-targets`, root crate) before each PR. Live proof on a real daemon for F3 (`/attach` an image over adopt) and G1 (`mode = auto` with tiny thresholds → `ResponseReset` renders).
- Adversarial review (shady) after H, after F3, after G3, after E P4 and P7.
- No author names in commits, PR text, or docs.

## Order and stacking
1. **H** → PR on dev (small, ~50 production lines + tests). Merge.
2. **F** and **G** in parallel → two PRs on dev. Merge.
3. **E P0–P2** → PR on dev (dark plumbing). Merge.
4. Answers to E §5 → **E P3–P10** → PR on dev.
5. Release: all five features complete. Version bump then.

## Gates
| After | Bar |
|---|---|
| H | engine ≥ 2170 tests, 0 new failures; `symbol-audit.py` production residue == documented deliberate list |
| F, G | workspace ≥ 4463 + new, 0 new failures; live proofs above |
| E P4 | shady review; `tests/autonomous_plugin.rs` 7/7 still green through the actor |
| E P7 | the 12 gate tests (S1 zero-client suspend/timeout, S3 caps, S2/S5 grant, S9 no auto-approve) |
