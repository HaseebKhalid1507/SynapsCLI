# SynapsCLI — Full Repository Review

**Date:** 2026-04-20
**Reviewer:** Claude Opus 4.7
**Scope:** Complete repository (`/home/haseeb/Documents/Projects/SynapsCLI`)
**Revision reviewed:** `main` @ commit `4fdc53f` (post single-binary consolidation)
**Lines of Rust reviewed:** ~21,300 across 89 `.rs` files

---

## 1. Executive Summary

SynapsCLI is a Rust-native AI agent runtime that embeds Claude via the
Anthropic API into terminal workflows. The recent binary consolidation
(commit `e1965b6`) unified eight separate binaries into a single `synaps`
binary with subcommand dispatch. The architecture is clean, the hot path is
well-engineered, and `AGENTS.md` is one of the better developer guides seen
in a project of this size.

**Overall verdict:** production-quality foundation with a short list of
concrete fixes needed before wider distribution. None of the findings block
current internal use. The highest-impact items are a symlink-dereference
class of filesystem issues and the chronic five-site settings sync that
makes the codebase harder to evolve than it needs to be.

**Counts by severity**

| Severity | Count |
| -------- | ----- |
| Critical | 0     |
| High     | 4     |
| Medium   | 5     |
| Low      | 4     |
| Nit      | 4     |

No Critical findings; the codebase is defensively written and the hot path
is solid. The High findings are all tractable inside a single sprint.

---

## 2. Repository Map

### 2.1 Purpose and positioning

A Rust-native alternative to Node/Python-based AI CLIs. Core pitch: sub-
100 ms cold start, single static binary, thoughtful prompt-cache strategy
(~90% hit rate via manual breakpoints vs ~53% with automatic caching).

### 2.2 Binary shape (post-consolidation)

```
synaps                 # default → TUI
synaps run <prompt>    # one-shot
synaps chat            # headless streaming over stdio
synaps server --port P # WebSocket server
synaps client --url W  # WebSocket client
synaps agent --config  # autonomous worker (spawned by watcher)
synaps watcher [cmd]   # supervisor daemon
synaps login           # OAuth flow
```

Dispatch lives in `src/main.rs` via clap; each subcommand has a
corresponding `src/cmd_*.rs`. The TUI, previously `bin/chatui.rs`, is now a
library module at `src/chatui/mod.rs`.

### 2.3 Module layout

| Module        | Role                                                            | Approx LOC |
| ------------- | --------------------------------------------------------------- | ---------- |
| `runtime/`    | API body construction, SSE parsing, streaming, tool dispatch    | 1,739      |
| `chatui/`     | TUI event loop, draw, markdown, themes, settings UI             | 3,734      |
| `tools/`      | Built-in tools + registry                                       | 1,920+     |
| `core/`       | Config, models, session, auth, protocol, errors                 | 1,721      |
| `cmd_*.rs`    | Per-subcommand entry points                                     | 1,663      |
| `watcher/`    | Autonomous-agent supervisor, IPC, display                       | ~700       |
| `mcp/`        | Model Context Protocol client (lazy spawn)                      | ~300       |
| `skills/`     | Skill/plugin discovery, registry, marketplace                   | ~400       |

### 2.4 Tech stack

- **Language:** Rust 1.80+, edition 2021
- **Async:** `tokio` (full features)
- **HTTP:** `reqwest` 0.11 (SSE streaming)
- **TUI:** `ratatui` 0.29 + `crossterm` 0.28 + `tachyonfx` 0.9 effects
- **Syntax highlighting:** `syntect` 5 with embedded themes
- **CLI parsing:** `clap` 4 (derive)
- **PTY:** `portable-pty` 0.9 for stateful shell sessions
- **WebSocket:** `axum` 0.7 + `tokio-tungstenite` 0.21
- **File locks:** `fs4` 0.13 (advisory locks on `auth.json`)
- **Release profile:** LTO, single codegen unit, strip, panic=abort

### 2.5 Documentation status

- `README.md` — product overview and quick start
- `AGENTS.md` (24 KB) — the onboarding doc; covers request lifecycle,
  adding tools/settings/themes, cache strategy, thinking config, known tech
  debt
- `CHANGELOG.md` — version history including the binary consolidation
- `CONTRIBUTING.md` — branch model, pre-submission checklist
- `docs/plans/`, `docs/specs/` — in-flight design docs

No `.github/workflows/` directory: the repo has no automated CI. Tests are
run locally with `cargo test --lib`.

---

## 3. What the project does well

These are not filler — they are the parts of the codebase worth preserving
as constraints on future work.

### 3.1 Adaptive-thinking sentinel handling

`core/models.rs` treats `thinking_budget == 0` as the "let the model decide"
sentinel. Every call site in `runtime/api.rs` checks it before falling
through to the legacy fixed-budget path. Adaptive-vs-legacy routing is
comprehensively unit-tested (`models.rs:111-207`). This is the kind of
invariant that usually rots; here it has clearly been defended.

### 3.2 Prompt-cache breakpoint placement

`runtime/helpers.rs` places cache breakpoints every four user messages and
enforces the 4-marker system limit. The math is correct despite the comment
being slightly misleading. The 90% hit-rate claim in the README is
plausible given this scheme.

### 3.3 SSE streaming and retry

`runtime/api.rs:126-176` retries 429/500/502/503/529 with exponential
backoff and breaks after `max_retries`. Partial SSE lines are buffered
correctly (lines 194-208) and the trailing-buffer edge case (lines 375-408)
is handled explicitly.

### 3.4 Tool cancellation

`runtime/stream.rs:227-236` uses `tokio::select!` to cancel mid-execution
and synthesize tool results so message history stays valid on Ctrl+C. No
orphaned state.

### 3.5 Concurrent auth-token refresh

`runtime/auth.rs:20-63` uses a fast read-lock path and delegates contested
refreshes to a cross-process `fs4` lock. Multiple `synaps` processes on the
same machine won't duplicate refreshes or race the token file.

### 3.6 Child process hygiene

Both the bash tool and MCP connections use `kill_on_drop(true)`. ANSI is
stripped from bash output before returning. Output is truncated rather than
unbounded.

### 3.7 AGENTS.md

Unusually complete for a project this size. The "Common Pitfalls" section
documents the five-site settings sync, the `thinking_budget: 0` sentinel,
and the PTY parallel-test issue — meaning future contributors have a
fighting chance of not re-discovering these the hard way.

---

## 4. Findings

Each finding has severity, location, description, and a concrete
remediation. Severities:

- **High** — fix before the next external release
- **Medium** — fix within the current sprint
- **Low** — worth addressing when next in that file
- **Nit** — optional polish

### 4.1 High

#### H1. Command list duplicated across two modules

**Files:** `src/chatui/commands.rs:13`, `src/skills/mod.rs:49`

`ALL_COMMANDS` (used by slash-command dispatch) and `BUILTIN_COMMANDS`
(used by tab-complete) are two independent 13-entry arrays. No test
enforces equivalence. Drift silently breaks either dispatch or
auto-completion.

**Fix:**

```rust
// src/skills/mod.rs
pub const BUILTIN_COMMANDS: &[&str] = &[ /* canonical list */ ];

// src/chatui/commands.rs
pub use crate::skills::BUILTIN_COMMANDS as ALL_COMMANDS;
```

Add a test that iterates `ALL_COMMANDS` and confirms every entry resolves
to a branch in `handle_command`.

Estimated effort: **15 minutes**.

---

#### H2. Five-site settings synchronisation

**Files (all five must stay in lock-step):**

1. `src/chatui/settings/schema.rs` — `SettingDef` entry
2. `src/chatui/mod.rs::apply_setting` — runtime mutation
3. `src/core/config.rs::load_config` — config-file parsing
4. `src/chatui/commands.rs::ALL_COMMANDS` — slash-command surface
5. `src/skills/mod.rs::BUILTIN_COMMANDS` — tab-complete registry

The existing schema test catches drift between (1) and (3) only. Adding a
setting that forgets (2), (4), or (5) compiles cleanly and silently loses
functionality at runtime.

**Fix options, in order of increasing ambition:**

- **Cheap:** a single test that walks `ALL_SETTINGS` and for each entry
  asserts the key is present in `apply_setting`'s match arms (via a
  hand-maintained list inside the test, but still centralised).
- **Better:** a `setting!` declarative macro that expands to the schema
  entry, the `apply_setting` arm, and the config parser clause from one
  definition.
- **Best:** derive-macro on a single `Settings` struct that generates all
  five surfaces. Heavier investment but retires the class of bugs
  permanently.

H1 is a prerequisite for any of these: having two command lists already
means the macro has to emit into two places.

Estimated effort: **2 hours for the cheap fix; 1 day for the macro**.

---

#### H3. Tools follow symlinks unconditionally

**Files:** `src/tools/read.rs:42`, `src/tools/write.rs:47-55`, `src/tools/edit.rs:46-75`

`tokio::fs::read`, `::write`, and the edit tool's read-modify-rename
sequence all dereference symlinks. A malicious or merely clumsy prompt can
read files outside the intended working directory via a pre-planted
symlink (e.g., `~/.ssh/id_rsa` linked from somewhere under the project),
and can clobber files outside the project by writing through one.

**Impact:** In a consent-to-shell tool the bar is not "zero filesystem
access" but "no surprising filesystem access". A symlink in a checked-out
repository is exactly that kind of surprise.

**Fix:**

1. In each tool, call `fs::symlink_metadata()` before the operation.
2. If the entry is a symlink, either reject ("refusing to follow symlink
   at {path}; pass --follow-symlinks to override") or canonicalise and
   re-check the result stays within an allowlist root.
3. For `edit.rs` specifically, also harden the read-modify-rename against
   TOCTOU: re-check inode after rename, or hold an exclusive `fs4` lock
   on the target for the duration.

Estimated effort: **2-3 hours**.

---

#### H4. Session writes are non-atomic

**File:** `src/core/session.rs:73-79`

`tokio::fs::write(path, json)` writes in place. A crash mid-write
truncates or corrupts the session file. On next launch, `serde_json`
returns an error and the user loses the session history.

The codebase already has an atomic-write pattern (`core/config.rs` and
`cmd_agent.rs`). It just wasn't applied here.

**Fix:**

```rust
let tmp = path.with_extension("tmp");
tokio::fs::write(&tmp, &json).await?;
tokio::fs::rename(&tmp, &path).await?;
```

While in this file, also add a recovery path in the loader: on parse
error, rename the corrupted file to `<id>.bak` and surface a clear error
to the caller rather than `exit(1)`.

Estimated effort: **30 minutes**.

---

### 4.2 Medium

#### M1. Opus pricing inconsistent across two code paths

**File:** `src/cmd_server.rs:40-45`

`cmd_server.rs` uses `(15.0, 75.0)` for Opus per-million-token rates, but
`src/chatui/app.rs:255-263` correctly uses `(5.0, 25.0)` (which matches
Anthropic's current Opus 4.5/4.6/4.7 pricing — Opus dropped from the old
$15/$75 tier when 4.5 shipped). The server's cost counter thus over-
reports by 3× relative to the TUI counter, and both paths have their own
hard-coded table that will drift again at the next pricing update.

**Fix:** Introduce `pricing_for_model(&str) -> (f64, f64)` in
`core/models.rs`. Replace both the `cmd_server.rs` match and the
`app.rs::record_cost` match with calls to it. Add a unit test that pins
the expected rates per model.

Estimated effort: **30 minutes**.

---

#### M2. Tool input JSON failures fall through to `{}`

**File:** `src/runtime/api.rs:299`, `:391`, `:421`

When a `tool_use` block's input JSON fails to parse, the code substitutes
`json!({})`. The tool then runs with empty input, usually fails, and the
downstream error is indistinguishable from a legitimate tool failure.

**Fix:** Capture the parse error and synthesize a `tool_result` with
`is_error: true` and a message like
`"invalid tool input JSON: <parser error>"`. The model sees the real
problem on the next turn.

---

#### M3. `unreachable!()` in the draw hot path

**File:** `src/chatui/draw.rs:144`, `:250`, `:256`

Three `unreachable!()` calls on theme-colour lookups. If theme loading
ever produces a `Theme` missing one of those colours (user-supplied
theme, future theme with a typo), the TUI panics mid-render.

**Fix:** Replace each with `.unwrap_or_else(|| Color::Rgb(128, 128, 128))`
or similar neutral fallback. A broken theme should degrade visually, not
crash.

---

#### M4. `grep` regex comes straight from the model

**File:** `src/tools/grep.rs:65`

The pattern is handed to the `grep` subprocess untouched. Catastrophic
backtracking in grep is uncommon but possible with pathological patterns.

**Fix:** Pre-compile the pattern with the `regex` crate (or run
`grep -P --timeout`) and tighten the overall tool timeout from 15 s to
5 s. Small blast radius, but cheap hardening.

---

#### M5. WebSocket server has no auth when bound off loopback

**Files:** `src/main.rs:47`, `src/cmd_server.rs:142`

Default `--host` is `127.0.0.1` (verified at `main.rs:47`), so the safe
case is safe. However, a user who passes `--host 0.0.0.0` (or a LAN
address) exposes full runtime control — `/ws` accepts `ClientMessage`
frames that drive the agent — with no authentication, no TLS, and no
rate-limiting. The current behaviour silently binds and prints the URL.

**Fix:**

1. When `host` is non-loopback, require an explicit `--allow-network`
   flag; refuse otherwise with an explanatory error.
2. Add minimal bearer-token auth on `/ws`: generate a fresh token per
   server start, print it to stderr, require it as a subprotocol or
   query-param on the upgrade.
3. Document in README that the server is dev-only and not hardened for
   hostile networks.

---

### 4.3 Low

#### L1. Dead-code suppressions without rationale

**Files:** `src/runtime/mod.rs:45-47`, `src/runtime/types.rs:73`,
`src/chatui/plugins/state.rs:1`, others

Eleven `#[allow(dead_code)]` sites across the crate. Most have a
justifying comment; a handful don't. Either add a one-liner or delete the
field.

---

#### L2. Server startup banner mis-aligns on multi-digit ports

**File:** `src/cmd_server.rs:145-150`

The fixed-width box drawn with Unicode box characters assumes specific
string widths. `--port 80` and `--port 12345` both break alignment.
Cosmetic.

---

#### L3. Cache-breakpoint comment is misleading

**File:** `src/runtime/helpers.rs:55`

The position math is correct; the comment suggests a different semantics.
Reword the comment to match what the code actually does ("position within
`user_indices`, not the main message list").

---

#### L4. `find` tool timeout is 10 s, inconsistent with other tools

**File:** `src/tools/find.rs:57`

Bash is 30 s (300 s max), grep 15 s, find 10 s. Unify to a shared
`DEFAULT_TOOL_TIMEOUT` constant.

---

### 4.4 Nit

- **N1.** `tokio::select!` tick guard in `src/chatui/mod.rs:234` is
  visually dense; extract to a named helper.
- **N2.** `THINKING_BUDGETS` mappings appear in
  `src/chatui/mod.rs:48-77`, `src/chatui/commands.rs:135-156`, and
  implicitly in `src/core/config.rs::parse_thinking_budget`. Consolidate
  to a single `const &[(&str, u32)]` in `core/models.rs`.
- **N3.** A single `[[bin]]` stanza in `Cargo.toml` is correct post-
  consolidation, but the stanza is sandwiched between pre-consolidation
  comments; tidy them.
- **N4.** No `rust-toolchain.toml` pinning the toolchain version. Add
  one pinned to the version used in development so contributors and CI
  agree.

---

## 5. Security posture

Because SynapsCLI is a consent-to-shell tool (the user explicitly grants
the agent shell and filesystem access), several items that would be
high-severity security issues in a web service are *by design* here:

- `bash -c` executing arbitrary strings from the model: intended.
- Read/write access to the user's home directory: intended.
- No sandbox: intended.

Those are product requirements, not bugs. The items in §4 that **do**
carry security weight — because they violate the "no surprising access"
principle rather than the "no access" principle — are:

- **H3** (symlink dereference)
- **M5** (WebSocket server with no auth when bound off loopback)

Note: subagent recursion is **not** unbounded — `ToolRegistry::without_subagent()`
in `src/tools/registry.rs:41` strips the `subagent` tool from the inner
runtime built at `src/tools/subagent.rs:116`. A subagent literally cannot
see the tool that spawned it. Initial draft listed this as a finding; on
verification the guard is already in place and works.

Defence-in-depth items already present and worth keeping: PKCE on OAuth
with state-parameter CSRF check (`core/auth/mod.rs:178`); `0o600`
permissions on `auth.json` (`core/auth/storage.rs:47-50`); `fs4` advisory
locks on the auth file; `kill_on_drop(true)` on spawned children.

---

## 6. Testing and CI

**Current state**

- ~224 unit tests, co-located as `#[cfg(test)] mod tests` at file bottoms.
- Command: `cargo test --lib -- --test-threads=1`. Single-threaded is
  required because seven PTY tests in `tools/shell/pty.rs` contend on TTY
  file descriptors.
- No integration-test harness, no property tests, no CI pipeline.

**Gaps**

- No CI means nothing stops a PR from landing with a broken `cargo build`
  or `clippy` warning.
- Pricing, `KNOWN_MODELS`, and thinking-budget mappings change whenever
  Anthropic ships a new model. A scheduled CI job that exercises the
  happy path against the real API would catch drift early.

**Recommended CI (start minimal, expand later)**

```yaml
name: ci
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo build --all-targets
      - run: cargo test --lib -- --test-threads=1
      - run: cargo clippy --all-targets -- -D warnings
      - run: cargo fmt --check
```

Estimated effort to set up: **1 hour**, plus however long the first
`clippy -D warnings` run takes to clean.

---

## 7. Remediation plan

Grouped by effort and ordered by value-per-hour.

### 7.1 Quick wins (under 30 minutes each)

| Item | Files                                                       |
| ---- | ----------------------------------------------------------- |
| H1   | `src/chatui/commands.rs`, `src/skills/mod.rs`               |
| H4   | `src/core/session.rs`                                       |
| M3   | `src/chatui/draw.rs`                                        |
| L1   | ~11 sites                                                    |
| L3   | `src/runtime/helpers.rs`                                    |
| L4   | `src/tools/find.rs`                                         |
| N2   | `src/core/models.rs`, `src/chatui/mod.rs`, `src/chatui/commands.rs`, `src/core/config.rs` |

### 7.2 Half-day items

| Item | Files                                                       |
| ---- | ----------------------------------------------------------- |
| H3   | `src/tools/read.rs`, `src/tools/write.rs`, `src/tools/edit.rs` |
| M1   | `src/cmd_server.rs`, `src/chatui/app.rs`, `src/core/models.rs` |
| M2   | `src/runtime/api.rs`                                        |
| M4   | `src/tools/grep.rs`                                         |
| M5   | `src/cmd_server.rs`, `src/main.rs`                          |
| CI   | `.github/workflows/ci.yml` (new)                            |

### 7.3 Larger investments

- **H2 (settings macro):** 1 day for the macro; ~2 hours for the cheap
  test-only variant.
- **Optional:** replace the legacy `src/tools/agent.rs` with `subagent.rs`
  and delete after a deprecation window.

### 7.4 Suggested order

1. H1, H4, M1, M3 — quickest user-visible wins.
2. CI setup — prevents regression from here on.
3. H3, M5 — security-tinged items.
4. M2, M4 — correctness polish.
5. H2 — retire the settings drift class.
6. Low / Nit items opportunistically.

---

## 8. Architectural observations (not findings)

These aren't defects but are worth recording before the next major
refactor.

- **`Runtime` is getting big.** `src/runtime/mod.rs` is ~531 lines and the
  `Runtime` struct carries ~28 KB of state. When it crosses ~800 lines,
  consider splitting `RuntimeConfig` (immutable) from `RuntimeState`
  (mutable).
- **The watcher subsystem is under evaluation for removal** (per
  `AGENTS.md`). Avoid deep refactors in `src/watcher/` until that
  decision lands.
- **Theme changes require restart.** Swapping to hot-reloading themes
  needs the render state to hold a shared reference (`Arc<ArcSwap<Theme>>`
  or `Rc<RefCell<Theme>>`) rather than a captured `Theme`. Documented,
  not a bug, but worth scheduling for a UX sprint.
- **MCP tool registration uses a `tool_register_tx` channel to break the
  `Arc<ToolRegistry>` cycle.** Works, but unusual. A future refactor
  could flatten this into a plain `Vec<Box<dyn Tool>>` behind a `RwLock`.
- **Single binary, many modes.** Consolidation is a net win but means
  the `cmd_*.rs` entry points each build against the full crate. If
  binary size or cold-start start to drift, feature-gating per-subcommand
  modules is the escape hatch.

---

## 9. Appendix A — File-level high-value index

The twenty files most worth knowing when navigating this repo.

| File                               | Role                                                               |
| ---------------------------------- | ------------------------------------------------------------------ |
| `src/main.rs`                      | Clap dispatch, subcommand entry                                    |
| `src/lib.rs`                       | Crate root re-exports                                              |
| `src/runtime/mod.rs`               | `Runtime` struct, orchestration                                    |
| `src/runtime/api.rs`               | API body, SSE parsing, thinking config                             |
| `src/runtime/stream.rs`            | Tool dispatch from `ToolUse` blocks                                |
| `src/runtime/helpers.rs`           | Cache breakpoint placement                                         |
| `src/runtime/auth.rs`              | Auth headers, token refresh                                        |
| `src/core/models.rs`               | `KNOWN_MODELS`, thinking gating, pricing (post-H1)                 |
| `src/core/config.rs`               | `SynapsConfig`, `load_config`                                      |
| `src/core/session.rs`              | Session persistence                                                |
| `src/chatui/mod.rs`                | TUI event loop, `apply_setting`                                    |
| `src/chatui/app.rs`                | App state, cost tracking                                           |
| `src/chatui/draw.rs`               | Render dispatch                                                    |
| `src/chatui/commands.rs`           | Slash commands                                                     |
| `src/chatui/settings/schema.rs`    | `SettingDef` / `ALL_SETTINGS`                                      |
| `src/tools/registry.rs`            | `ToolRegistry`, recursion-guard variant                            |
| `src/tools/subagent.rs`            | Subagent dispatch                                                  |
| `src/tools/bash.rs`                | Shell execution                                                    |
| `src/cmd_agent.rs`                 | Autonomous agent entry                                             |
| `src/watcher/supervisor.rs`        | Agent lifecycle, limits, handoff                                   |

---

## 10. Appendix B — Finding index (cross-reference)

| ID | Severity | File                                 | Est. fix   |
| -- | -------- | ------------------------------------ | ---------- |
| H1 | High     | `chatui/commands.rs`, `skills/mod.rs`| 15 min     |
| H2 | High     | five sites                           | 2h / 1d    |
| H3 | High     | `tools/{read,write,edit}.rs`         | 2-3 h      |
| H4 | High     | `core/session.rs`                    | 30 min     |
| M1 | Medium   | `cmd_server.rs`, `chatui/app.rs`, `core/models.rs` | 30 min |
| M2 | Medium   | `runtime/api.rs`                     | 45 min     |
| M3 | Medium   | `chatui/draw.rs`                     | 15 min     |
| M4 | Medium   | `tools/grep.rs`                      | 30 min     |
| M5 | Medium   | `cmd_server.rs`, `main.rs`           | 1 h        |
| L1 | Low      | ~11 sites                            | 30 min     |
| L2 | Low      | `cmd_server.rs`                      | 10 min     |
| L3 | Low      | `runtime/helpers.rs`                 | 5 min      |
| L4 | Low      | `tools/find.rs`                      | 5 min      |
| N1 | Nit      | `chatui/mod.rs`                      | 10 min     |
| N2 | Nit      | `core/models.rs` and callers         | 30 min     |
| N3 | Nit      | `Cargo.toml`                         | 5 min      |
| N4 | Nit      | `rust-toolchain.toml` (new)          | 5 min      |

Total estimated effort for everything excluding H3-macro and CI polish:
**~12 hours of focused work**.
