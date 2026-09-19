# #112 scoping — synthesis (S326, 2026-09-19)

Four Opus recon deliverables in this directory (A engine merge · B driver port · C product · D security), ~150 KB, every claim cited file:line. This page is the decision layer. Read the four when you want the evidence.

## The one-line verdict

**Land JR's engine half now (dark, flag-gated, ~28 h). Do NOT port the driver into the actor until three things exist that today do not: session env identity, a prompt policy for zero-client sessions, and spend caps.** All four analysts arrived at this independently.

## What #112 actually is (FACT, A §1/§7)

Three features on one branch; only one is TUI-coupled:

| Feature | Where | Merges onto daemon dev? | Default state after merge |
|---|---|---|---|
| Durable context: rollover at pressure, archive, retrieval, recovery, `session_save_order` | agent-core + agent-engine (`continuation.rs`, `context_head.rs`, `context_archive.rs`, `tools/context_checkpoint.rs`) | Yes — 10 engine conflict files, all mapped | **dark** (`context_management.mode = off`) |
| Axel forums + shared memory backend + `axel-memory-service` sidecar | agent-core `memory/`, agent-engine `memory_backend/`, `sidecars/` | Yes — pure addition, 0 conflicts; sidecar is per-operation spawn, not long-lived | **dark** (`backend = legacy`) |
| Multimodal attachments (engine) | `runtime/attachments.rs`, `attachments.rs` | Yes — `RpcAttachment` wire type identical to dev's | inert until `/attach` |
| Session driver **policy** (Grant/Proposal/Reply/classify/validate) | agent-engine `extensions/session_driver.rs` (1,887 ln) | Yes — pure types, 0 conflicts | inert (no host arms it) |
| Session driver **host loop** (timers, steering FIFO, revoke, observe_terminal) | agent-tui `session_driver.rs` (1,811 ln) + 7 TUI files + `cmd/chat.rs` inline loop | **No** — drives `Runtime` in-process; dev's TUI is a thin client (Wall 2, 59 errors) | — |

JR already did the split for us: policy in the engine, loop in the TUI. The loop is the only thing that can't land.

## Wall 1 is solved on paper (A §3)

`ContextHeadCheckpoint` carries a `oneshot::Sender` the frontend completes after saving. Under dev the actor owns saves, so: the actor intercepts the checkpoint **before serialization**, runs `ContextHeadPersistence::persist(checkpoint.messages)` on its own task, completes the receipt, and only then publishes the new window. Ordering checked against `session_save_order` (JR's lock must wrap *all* dev save paths — risk #3), park (no race: streaming must end before park fires), the F10 journal flock (same file, already covered), reload (continuation state restores from the private marker in `api_messages`), LinkedSuccessor (reset continuation on id change), LocalTransport (nothing special). ~3 h. Not a design question anymore.

## Merge plan for the engine half (A §8) — 28 h base, 36 h with buffer

Phases 0–10: Cargo align → agent-core additions → `runtime/mod.rs` (L, RuntimeParts field audit is risk #2) → **`runtime/stream.rs` (L, 5 h — 1,000+ added lines interleaved with our session_id threading; the single riskiest merge; F19 fix lands here in 5 lines since JR's advisory code is spatially separate)** → manager + driver types → tools → memory backend + sidecar → attachments → `engine/setup.rs` → Wall 1 → live verification (daemon + park + reload + lock + `/context auto` on Enigma-sized history). All JR's integration tests run unchanged on the merged tree (A §6). Whole-workspace bar on bella at every phase; S235 rule (no `--theirs`) throughout.

**Known daemon-specific break (A §4, risk #4):** `MemoryBinding::configured_current()` scopes memory by *process cwd* → every session gets the daemon's project. That is session-identity T1–T3 territory; land the engine half dark and fix binding-by-session-cwd as part of T1, not as a merge hack.

## Why the driver waits (B §Q3/Q5, D S1–S3)

| Blocker | Evidence | What must exist first |
|---|---|---|
| **Zombie prompt with zero clients** | `can_park()` requires `pending_prompts.is_empty()` (actor.rs:701); a `Prompt{Confirm}` mid-driver-turn with `clients=0` blocks the stream on a oneshot forever — can't park, can't answer, keeps the Runtime resident. With `auto_approve_confirms` (S9) it instead auto-approves headless. | A **prompt policy** for unattended sessions: default = suspend the turn, notify (F9-style), time out → revoke grant. Never auto-approve under a driver. |
| **Plugin is single-tenant** | `examples/extensions/autonomous/main.py:326` has one `self.run`; `ExtensionManager` is one `Arc` per daemon (host.rs:51); `PollRequest` carries no `session_id`. Two sessions arming `/auto` overwrite each other's run. | Either a daemon-wide "one driver grant per plugin" lock, or `session_id` in the driver frames + plugin keyed by it (breaks JR's zero-plugin-change goal — decision). |
| **No spend controls anywhere** | No per-run / per-session / per-daemon cost cap in the codebase; `max_duration_ms` is time, optional, and `None` = unbounded. `--turns` is plugin-advisory. | Hard caps at the actor (USD via existing `session_cost`), daemon aggregate, audit log of every grant/decision outside the journal (S13). Table stakes before Praxis sees it. |
| **Wrong env** | F25: daemon-frozen env. An autonomous run installing into the wrong Python or hitting the wrong AWS account is strictly worse than an interactive one. | Session-identity T1–T3 (+T5 for park). |
| **Silent turn loss** | F19: no human watching to notice a dropped turn. | F19 fix (5 lines in the stream.rs merge). |

Port itself (B §Q7): 42–55 h. Three new commands (`DriverStart`, reuse `Steer`/`Cancel`), 2–3 new events (`DriverArmed`, `DriverRevoked`, `DriverTurnOutcome`), driver state machine (grant, deadline, pending callback, steering FIFO, feedback ring) moves onto the actor; TUI shrinks to "render driver state, send start/steer/stop". Riskiest step is porting `tick()` (~10 h) — de-risk by writing it arm-by-arm with synthetic tests before wiring the select loop. Zero plugin contract changes *unless* we pick multi-tenancy by `session_id`.

Semantics that change and need a decision (B §Q3): grant survives detach (yes — that's the feature, but F27's quit-notice must land first); Esc/Ctrl-C = stop-driver-then-detach, not detach; grant owned by the input owner, not the creator; park while armed = never (auto keep-warm while a grant exists); daemon reload = revoke (JR: "process restart must never restore a run" — a re-exec is a restart); `synaps send` while armed = steering only if from the owner… (D S4 says today it triggers auto-turns — that's a bug regardless).

## Product read (C)

Durable context is the real differentiator: **rollover with retrievable original evidence instead of summarization** — nobody local (jcode, pi, Claude Code, Codex per recon) does this; the weak spot is JR's own "no measured 2× reduction" — we need a benchmark before we claim it. Driver: most sophisticated design in the field (permissioned, fail-closed, human steering outranks the goal, wall-clock checkpoints) but TUI-only today; actor-native + daemon = "close the lid, work continues, steer from your phone, N runs on one sidecar set" — genuinely nobody has this, *and* it's exactly where the safety gaps above live. Daemon numbers (2.5 MB client, 3.2 MB marginal/session, turns survive terminal close) are measured and ahead of jcode; behind on Windows and remote daemon.

## Recommended sequence

1. **Merge #115 → #116 → dev** (done, tested, unblocks everything).
2. **Session-identity T1–T3** (~3 days) — includes memory-binding-by-session-cwd. Differential test is the proof.
3. **Engine half of #112** onto that (28–36 h), F19 inside the stream.rs pass, Wall 1 actor-owned. Lands dark. JR's 4,265 tests + ours green on bella. Live: `/context auto` on a real long session in the daemon, park/unpark/reload with an archive on disk.
4. **T4/T5/T8/T9/T10** parallel (identity wave 2 + F27 quit notice + F23 + F24).
5. **Driver prerequisites**: prompt policy for zero-client sessions; spend caps + audit log; single-tenancy decision. Spec first, JR reads it (this is his feature's contract changing).
6. **Driver → actor** (42–55 h). Then update `session-drivers.md` "local TUI only" → "any session".
7. T6/T7 extension env protocol, T11 peer-cred, alongside 5–6.

## Decisions for Haseeb

1. **Engine half first, dark** — agree? (The alternative — hold all 50k lines until the driver port is done — is 3+ more weeks of drift.)
2. **F19 inside the merge pass** (5 lines, same file) or a separate PR for attribution?
3. **Driver multi-tenancy**: one grant per plugin daemon-wide (zero plugin changes, JR-faithful) or `session_id` in the driver frames (correct for N sessions, plugin must change)?
4. **Zero-client prompt policy**: suspend+notify+timeout-revoke (my recommendation) vs deny-and-revoke immediately?
5. **Spend cap shape**: per-run USD from the plugin's Start + per-daemon ceiling in config? Who sets the daemon ceiling on Praxis VMs?
6. The 4 session-identity questions still open from the earlier plan.
