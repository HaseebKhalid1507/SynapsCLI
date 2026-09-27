# Plan: Session process identity — "the thing I type is the thing that runs"

**Status:** draft for Haseeb review · **Origin:** S326 soak F25/F26/F27/F23/F24 · **Branch base:** `integration/daemon-soak-fixes` (PR #116)

## Problem

Under daemon mode, a session's *process identity* — env, path resolution, lifetime on quit — comes from the daemon, not the client that created it. Users have no mental model of a daemon; they expect tmux/emacsclient semantics: **every session behaves as if it were a fresh process spawned from my shell.** Today:

- `cd proj && source .venv/bin/activate && synaps` → agent `bash` has no venv, daemon's `PATH`, no `AWS_PROFILE`, dead `SSH_AUTH_SOCK` (F25, verified).
- `synaps --system ./prompt.md` → resolved against the daemon's cwd → silently ignored (F26, verified).
- Ctrl+C = detach; the turn keeps running headless (F27, verified). In-process it aborted.
- Reload with a lock-held journal aliases the id to an empty impostor (F23, verified).
- `--continue <pre-compaction id>` in-process forks silently; `compacted_into` exists but is never followed (F24, code-read).

`SessionConfig`'s own doc comment promises an "env overlay" that was never built. jcode (0.76) has the same hole — it forwards ~20 terminal vars for window routing only; its bash tool inherits the daemon env.

## Principle

**Process identity is a session fact, captured from the creating client at `Hello`, stored on the actor beside `cwd`, applied at every exec.** The daemon's own env is used by exactly one thing: the daemon.

Concretely: `Hello.cwd` already flows `Hello → SessionConfig → Runtime → ToolCapabilities → tools`. Env takes the **identical pipe**. Not an allowlist — an allowlist is a list of things you forgot (`VIRTUAL_ENV`, `CONDA_PREFIX`, `PYENV_VERSION`, `NVM_BIN`, `GOPATH`, `SSH_AUTH_SOCK`, `GPG_TTY`, `DOCKER_HOST`, `KUBECONFIG`, `DIRENV_*`, `NIX_*`, `http_proxy`…).

Semantics, decided up front:
| Question | Answer |
|---|---|
| Snapshot or live? | **Snapshot at session create.** `export FOO=1` after launch does not reach the session (a process wouldn't see it either). |
| Two clients, one session, different envs? | Session keeps the **creator's** env, same as cwd. `--takeover` does not change it. Attach line says so. |
| Reconnect after daemon reload? | Session env survives in the actor; rehydrate carries it. Reconnect does not re-send. |
| Park/unpark, daemon restart? | Env is **journaled minus secrets** (denylist). Unpark restores it. A tool needing a stripped secret fails loudly: `session env lost ANTHROPIC_API_KEY across park — reattach from a shell that has it`. Never silently fall back to the daemon's. |
| `SYNAPS_*` in the client env? | `SYNAPS_CLIENT_*`/`SYNAPS_TUI_*` are client-only and stripped before send. Everything else is session env. The daemon never reads session env for its own config. |
| In-process host? | `env: None` = inherit process env → **byte-identical to today**. Same pattern as `cwd: None`. |
| Empty/missing value? | Apply with `env_clear()` then `envs(session.env)`. A key absent from the session must not leak through from the daemon (jcode's `env_remove` discipline). |
| Extensions (shared sidecars)? | Per-call `session.env`+`session.cwd` in the hook/tool-call frame; SDK applies to subprocesses it spawns. Manifest flag `session_env: true` opts in; a plugin that declares subprocess use without the flag gets a **per-session instance** (correct, costs RAM) until it opts in. Built-ins opt in on day one. |

## Dependency graph

```
T1 wire+actor: Hello.env → SessionConfig.env → Runtime.env → ToolCapabilities.env
   ├── T2 bash/find/grep/ls apply env_clear().envs()          ← the user-visible fix
   │     └── T3 differential test (in-process vs daemon)      ← the guarantee
   ├── T4 --system/--prompt-manifest by content / canonical   (F26)
   ├── T5 journal env minus secrets + loud unpark              (park/restart)
   └── T6 extension protocol: per-call env/cwd + manifest flag (shared sidecars)
         └── T7 built-in plugins opt in; per-session fallback
T8 quit-while-streaming notice (F27)                            independent
T9 reload: no alias on lock-held; Parked placeholder (F23)      independent
T10 --continue follows compacted_into; predecessor lock (F24)   independent
T11 SO_PEERCRED uid check + SYNAPS_* split                       last
```

Risk-first: T1–T3 first (the promise), then T5 (the only place secrets touch disk), then T6 (the protocol bump — upstream in the loop).

---

## Task 1: Session env on the wire and the actor

**Description:** Add `env: Option<Vec<(String, String)>>` to `Hello` (client snapshot, `SYNAPS_CLIENT_*`/`SYNAPS_TUI_*`/`SYNAPS_DAEMON_*` stripped, `SYNAPS_MEM_TRACE*` stripped), `SessionConfig`, `Runtime` (beside `cwd`, `set_env/env()`), and `ToolCapabilities`. Daemon `conn.rs` copies `hello.env` into `config.env` exactly where it copies `cwd` (only on `Attach::Create`; never on `Existing`). `SessionActor::create` calls `runtime.set_env(cfg.env)`; unpark/rehydrate re-apply it. In-process hosts pass `None`.

**Acceptance criteria:**
- [ ] `Hello`/`SessionConfig` round-trip `env` through serde; absent → `None`; legacy frames without the field decode.
- [ ] `ToolCapabilities.env` is `Some(creator's env)` for daemon sessions and `None` for in-process, asserted in a unit test on the actor.
- [ ] A second client attaching to an existing session does not change `Runtime.env` (test).

**Verification:** `cargo test -p synaps-engine --locked` on bella; clippy `-D warnings`.
**Dependencies:** None · **Files:** `session/wire.rs`, `session/types.rs`, `daemon/conn.rs`, `runtime/mod.rs`, `tools/mod.rs`, `session/actor.rs`, `tui/attach.rs`, `cmd/attach.rs` · **Scope:** M

## Task 2: Apply session env at every tool exec

**Description:** In `tools/bash.rs`, `find.rs`, `grep.rs`, `ls.rs` (and any other `Command::new` in `tools/`): if `ctx.capabilities.env` is `Some`, `cmd.env_clear().envs(env)`; if `None`, leave inheritance untouched. Keep `TMPDIR`/tool-specific additions on top. Windows path (`powershell`) same treatment.

**Acceptance criteria:**
- [ ] Daemon session created from a shell with `JT_MARK=x VIRTUAL_ENV=/v PATH=/v/bin:$PATH`: `bash` tool reports `MARK=x VENV=/v`, `which python` → `/v/bin/python` (the exact H1 repro, now green).
- [ ] A var present in the daemon env but absent from the client is **not** visible to the tool (leak test).
- [ ] In-process behaviour unchanged (`env: None` → no `env_clear`).

**Verification:** unit tests with a fake capabilities env; manual H1 script on bella.
**Dependencies:** T1 · **Files:** `tools/bash.rs`, `tools/find.rs`, `tools/grep.rs`, `tools/ls.rs` · **Scope:** S

## Task 3: Differential test — in-process vs daemon must be indistinguishable

**Description:** New `tests/session_identity_differential.rs`, following `tests/tui_transport_differential.rs`. Synthetic env (`JT_A=1`, `PATH=/tmp/fake:$PATH`, a `*_KEY`), cwd `/tmp/x`, relative `--system ./p.md`. Run one echo-factory turn whose tool dumps `env | sort; pwd; sha256 of system prompt` (a) in-process, (b) through a daemon started from a *different* env/cwd. **Diff must be empty** modulo `SYNAPS_CLIENT_*`/`SYNAPS_MEM_TRACE*`. Locks F25/F26 shut permanently.

**Acceptance criteria:**
- [ ] Test fails on `integration/daemon-soak-fixes` HEAD (proves it detects F25), passes after T1+T2(+T4).
- [ ] Runs in `cargo test --workspace` under 10 s; serial (`serial_test`) because it spawns a daemon.

**Verification:** bella workspace run. **Dependencies:** T1, T2 · **Files:** `tests/session_identity_differential.rs` · **Scope:** S

### Checkpoint A (after T1–T3)
- [ ] Workspace green on bella; H1 script green on the real binary against a real daemon.
- [ ] Human review of the strip list and the `env_clear` decision before T5 touches disk.

## Task 4: Path-like args resolved client-side; `--system` by content

**Description:** In the thin-client path, before `Hello`: canonicalize `--prompt-manifest` against client cwd; for `--system`, if the value names a readable file, ship its **contents** (the journal already persists `system_prompt` as text). Error loudly if a path-like value doesn't exist (`--system ./x.md: no such file`) instead of treating it as prompt text. Same for `@file` attachments if they cross the wire.

**Acceptance criteria:**
- [ ] `cd /tmp/proj && synaps --system ./prompt.md` over adopt → session prompt is the file's contents (H2 repro green).
- [ ] `--system "You are X"` (non-path) still works; `--system ./missing.md` → non-zero exit with the path in the message.

**Verification:** unit test on the resolver; H2 script. **Dependencies:** T1 · **Files:** `tui/attach.rs`, `cmd/attach.rs`, maybe `config::resolve_system_prompt` · **Scope:** S

## Task 5: Journal env minus secrets; loud unpark when a secret is missing

**Description:** Persist `env` in the session header **after** a denylist filter (`*_KEY`, `*_TOKEN`, `*_SECRET*`, `*PASSWORD*`, `AWS_SECRET_*`, `*_CREDENTIALS`; reuse `memstat::sensitive` semantics, one shared helper in `agent-core`). Record the stripped **names** in `env_stripped: Vec<String>`. On unpark/rehydrate restore `env`; `ToolCapabilities` carries `env_stripped` so `bash` can emit a one-line system notice the first time a stripped name is referenced by a failing command (heuristic: non-zero exit + name appears in the script). Never fall back to the daemon's value.

**Acceptance criteria:**
- [ ] Journal on disk never contains a value for a denylisted key (test greps the file).
- [ ] Unpark restores non-secret env exactly (H1 after park → still `MARK=x`).
- [ ] Notice text names the variable and says "reattach from a shell that has it".

**Verification:** engine + core tests; park/unpark script on bella. **Dependencies:** T1 · **Files:** `agent-core/session.rs`, `agent-core/…/env_filter.rs` (new), `session/actor.rs` (park/unpark), `tools/bash.rs` · **Scope:** M

### Checkpoint B (after T4–T5)
- [ ] Full soak re-run of R2/R3/V-F2 on bella with env assertions added to the harness.
- [ ] Review secret denylist with Haseeb; decide whether `ANTHROPIC_API_KEY` in a *client* env should ever be session env at all (it probably should be stripped **and** ignored — the broker owns credentials).

## Task 6: Extension protocol — per-call session env/cwd + manifest opt-in

**Description:** Add `session: { id, cwd, env }` to the tool-call and hook frames sent to sidecars (`extensions/hooks/mod.rs:1117` currently sends `cwd: None`). Add manifest field `session_env: bool` (default false). Document in `docs/extensions/protocol.md` + a new `docs/extensions/session-env.md`: "if you spawn subprocesses, apply `session.env`/`session.cwd`; declare `session_env: true`." Bump protocol minor; additive, old sidecars ignore the field. Spec reviewed by upstream before merge (touches every Praxis plugin).

**Acceptance criteria:**
- [ ] A test sidecar that echoes `session.env.JT_MARK` returns the creator's value for two different sessions in one daemon (no bleed).
- [ ] Old sidecar (no field handling) still loads and runs.
- [ ] Docs updated; `docs/extensions/contract.json` regenerated.

**Verification:** extension e2e tests; manual with `web-tools` on bella. **Dependencies:** T1 · **Files:** `extensions/hooks/mod.rs`, `extensions/manifest.rs`, `extensions/runtime/*`, SDK (`synaps-skills` Python SDK — separate MR), docs · **Scope:** M (engine) + S (SDK)

## Task 7: Built-ins opt in; per-session fallback for unflagged spawners

**Description:** Update the shipped Python plugins that spawn subprocesses (`web-tools`, `synaps-tasks`?, `chronos`?) to apply `session.env`/`cwd` and set `session_env: true`. For plugins whose manifest declares `spawns_subprocesses: true` (new, honest field) **without** `session_env`, the daemon spawns one sidecar instance per session (existing per-session sidecar path from pre-daemon days) and logs why. Plugins that declare neither: shared, unchanged.

**Acceptance criteria:**
- [ ] `daemon status` lists sidecar instances with `shared` / `per-session(<n>)` and the reason.
- [ ] RAM: shared plugins still cost one instance; only flagged-unopted ones multiply.

**Dependencies:** T6 · **Files:** `extensions/manager.rs`, `extensions/loader.rs`, plugin manifests, `cmd/daemon.rs` · **Scope:** M

### Checkpoint C (after T6–T7)
- [ ] upstream sign-off on the protocol addition; Praxis plugin inventory checked for subprocess spawners.

## Task 8: Quit-while-streaming is loud (F27)

**Description:** In the thin TUI, on quit (Ctrl+C/`/quit`) while `app.streaming`: show `turn still running in the daemon — Esc to abort it first, or quit again to leave it running (synaps daemon sessions)`. Second quit within 3 s = detach. Line client: same text on stderr, exit as before. Document in `docs/daemon-mode.md` ("detach vs abort").

**Acceptance criteria:**
- [ ] First Ctrl+C mid-turn does not exit; message visible; Esc aborts; second Ctrl+C detaches.
- [ ] Ctrl+C while idle exits immediately (unchanged).

**Dependencies:** None · **Files:** `tui/mod.rs` (quit arm), `tui/input.rs`?, `cmd/attach.rs`, docs · **Scope:** S

## Task 9: Reload never aliases to an impostor (F23)

**Description:** In `daemon/reload.rs` rehydrate: when `SessionActor::create` fails with `SessionLockError::Held`, do **not** "recreate fresh"; register a `Parked` placeholder under the **same id** whose attach returns `AttachRefused { message: "journal locked by pid N (kind) — …" }`, and retry unpark on the next attach. Aliasing (`old → new`) is reserved for LinkedSuccessor. Log at `warn` with the holder.

**Acceptance criteria:**
- [ ] H5 script: after reload with X locked by an in-process runtime, `daemon sessions` shows X Parked (not a new id); `--attach X` while locked → refusal naming the pid; after the holder exits → attach succeeds with X's full history.
- [ ] No `recreating fresh` log line for lock-held journals.

**Dependencies:** None (F10 lock exists) · **Files:** `daemon/reload.rs`, `daemon/mod.rs`, `session/actor.rs` · **Scope:** S

## Task 10: `--continue` follows `compacted_into`; predecessor is not a fork point (F24)

**Description:** `resolve_session(id)`: if the loaded journal has `compacted_into: Some(succ)`, follow forward (loop, bounded) and emit `notice: <id> was compacted into <succ> — continuing there`. `SessionLock::try_acquire` for a journal with `compacted_into` set checks the successor's lock too. In-process `--continue <old>` while the successor is live in the daemon → the existing F10 refusal, naming the successor.

**Acceptance criteria:**
- [ ] Unit: chain of 3 journals resolves to the last; cycle guard.
- [ ] Integration: daemon session compacted (LinkedSuccessor, forced small threshold in test) → in-process `--continue <old>` refused naming the successor; after daemon stop → continues the successor, not the predecessor.

**Dependencies:** None · **Files:** `agent-core/session.rs` (`resolve_session`), `agent-core/session_lock.rs`, `engine/setup.rs` · **Scope:** S

## Task 11: Peer credential check and `SYNAPS_*` split (hardening)

**Description:** On accept, `SO_PEERCRED`: refuse a connection whose uid ≠ daemon uid (`Refused { reason: Protocol, message: "uid mismatch" }`). Define the `SYNAPS_*` split in one table in `docs/daemon-mode.md`: client-only (`SYNAPS_CLIENT_*`, `SYNAPS_TUI_*`, `SYNAPS_MEM_TRACE*`, `SYNAPS_DAEMON_*`), daemon-only (read at daemon boot), session (everything else, via T1). The strip list in T1 is generated from this table (one const).

**Acceptance criteria:**
- [ ] Test: a connection from a different uid (use `unshare`/`setpriv` in CI or skip if unavailable) is refused.
- [ ] The strip const has a test asserting every `SYNAPS_*` env read in `crates/agent-tui` and `src/main.rs` is in the client-only set (grep-based guard test, like the `SessionCommand Debug` grep guard).

**Dependencies:** T1 · **Files:** `daemon/listener.rs`, `daemon/conn.rs`, docs, `src/main.rs` · **Scope:** S

---

## Order & parallelism

| Wave | Tasks | Notes |
|---|---|---|
| 1 | T1 → T2 → T3 | sequential, one implementer; T3 written *first* as a failing test if the implementer prefers TDD |
| 2 | T4 ∥ T5 ∥ T8 ∥ T9 ∥ T10 | independent; five worktrees; all rebase on wave 1 |
| 3 | T6 → T7 | protocol bump; spec to upstream before code; SDK MR in `synaps-skills` |
| 4 | T11 | after everything else is stable |

Every task: build on bella only; `cargo test --workspace --locked` + clippy `-D warnings` on touched crates; live re-run of the matching soak script (`/tmp/jt-h*.sh` family, to be checked into `scripts/soak/`).

## Out of scope (tracked separately)
F19 empty-end_turn data loss (#377), F4/F5 reload-vs-in-flight-turn (#366), F7 incremental journal (#369), F6 abort-context-as-injection (#370), F8 orphan tool children on SIGKILL (#371), F18 zero-turn sessions never park (#376).

## Decisions (Haseeb, 2026-09-19 17:21: "go with your picks")
1. **Full client env**, not an allowlist. Strip only `SYNAPS_CLIENT_*`, `SYNAPS_TUI_*`, `SYNAPS_DAEMON_*`, `SYNAPS_MEM_TRACE*`.
2. **`*_API_KEY`/`*_TOKEN`/`*_SECRET*`/`*PASSWORD*`/`AWS_SECRET_*`/`*_CREDENTIALS` are stripped AND ignored** — never session env, never journaled. The broker owns credentials.
3. **T7: unflagged subprocess-spawning plugins get a per-session instance** + a logged warning. Never refuse to load.
4. **T8: second Ctrl+C within 3 s detaches**; first shows the notice. No y/n prompt.
