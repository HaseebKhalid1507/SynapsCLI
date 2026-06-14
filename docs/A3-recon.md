# A3 Crate Split — Reconnaissance Report

**Branch:** `refactor/a3-crate-split`
**Target:** `~/Projects/agent-runtime`
**Plan source:** `docs/REVIEW-S205.md`, recommendation A3
**Status:** Read-only recon. No files modified.

> **TL;DR.** The TUI layer is already cleanly separable — nothing in the library reaches up into `tui/`. The real problem lives one level deeper: there is a **mutual dependency cycle between `runtime/`, `extensions/`, and `tools/`** (with `events/` and `core/` participating on smaller back-edges). The four-crate plan as written (`agent-core` / `agent-providers` / `agent-tui` / `agent-runtime` bin) cannot be implemented as a single Cargo.toml surgery — `agent-providers` would need to import `extensions/`, `tools/`, `events/`, and `skills/`, none of which fit the spec's `agent-providers = "reqwest, SSE, provider/API engine"` description. The split needs an interstitial crate (or careful trait-extraction passes) before any Cargo.toml change. Detailed findings below.

---

## 1. Module Inventory

`src/lib.rs` (lib name `synaps_cli`, package `synaps`) declares only the **library-side** modules. `tui/`, `cmd/`, and `watcher/` are bin-only (`src/main.rs:23-25`) and never enter the library tree.

### Library modules (`pub mod` in `src/lib.rs`)

| Module | LOC | Role |
|---|---:|---|
| `core/` | 6 315 | Config parsing, session model, auth/OAuth, logging, error types, RPC protocol + dispatch, watcher-handoff types, model catalog metadata, chain helpers, conversation compaction. The "intended leaf." |
| `runtime/` | 10 832 | `Runtime` struct + reqwest HTTP client, Anthropic + OpenAI provider engines, SSE parser/types, request building, streaming loop, subagent registry/state, telemetry. **The proposed `agent-providers` core.** |
| `tools/` | 6 507 | `Tool` trait, `ToolRegistry`, built-ins (bash/read/write/edit/grep/find/ls), shell PTY session manager, secret-prompt channel, extension-bridge tool, subagent control tools, watcher-exit tool. |
| `extensions/` | 9 607 | External-process plugin runtime: manifest, capability/permission/trust/audit, JSON-RPC `process.rs`, hook dispatch bus, provider routing, settings editor, plugin task/widget surfaces. |
| `skills/` | 7 808 | Plugin/skill discovery, marketplace install, manifest parsing, slash-command registry, keybind registry, post-install runner, trust/state files. |
| `mcp/` | 620 | MCP (Model Context Protocol) client — JSON-RPC over stdio + lazy `mcp_connect` tool. |
| `events/` | 1 282 | Event bus — inbox watcher, per-session unix socket, event queue/types/registry, formatter for system-message injection. |
| `memory/` | 432 | Local-first JSONL memory store (leaf — see §3). |
| `engine/` | 997 | Boot/setup glue (`EngineOpts → SessionBoot`), command dispatch, stream wrapper, conversation state shared by TUI + headless. |
| `sidecar/` | 899 | Long-running plugin sidecar processes — discovery, manager, line-protocol, spawn. |
| `help.rs` | 1 039 | `/help` content + interactive find lightbox state machine (model only, no rendering). |
| `pricing.rs` | 196 | Per-provider token cost table + arithmetic. Leaf. |
| `toast.rs` | 46 | Headless toast queue (model only). Leaf. |

Library total: **~46 500 LOC.**

### Binary-only modules (declared in `src/main.rs`)

| Module | LOC | Role |
|---|---:|---|
| `tui/` | 20 205 | Ratatui app: `App`, draw loop, settings/plugins/models modals, syntect highlighter, markdown renderer, viewport, lightbox, signals, stream handler, plugin widgets, theme. **The proposed `agent-tui` core.** |
| `cmd/` | 3 464 | CLI subcommand entrypoints — `chat` (headless), `server` (axum WS), `agent` (autonomous), `login` (OAuth), `status`, `rpc` (line-JSON), `send`, `watcher`. |
| `watcher/` | 2 016 | Supervisor + IPC bridge + status display for the `synaps watcher` subcommand. |
| `bin/hidden/` | 6 455 | Second binary (`hidden`) — separate compilation unit. Not on the A3 critical path. |

Binary (default) total: **~25 700 LOC** including main.rs (195). Aligns with the "hot TUI edit recompiles ~25k" target in the brief.

---

## 2. Proposed Crate Assignment

Plan from `REVIEW-S205` recommendation A3:

| Crate | Role | Verdict |
|---|---|---|
| `agent-core` | types, config, errors, serde models | **Mostly clean** (2 back-edges to fix) |
| `agent-providers` | reqwest, SSE, provider/API engine | **Cannot fit `runtime/` as written** — see §3 |
| `agent-tui` | ratatui + syntect (all rendering) | **Clean — nothing in lib references `tui::`** |
| `agent-runtime` (bin) | glue | Receives `main.rs` + `cmd/` + `watcher/` + TUI run wiring |

### Per-module proposed placement

`✓` clean, `⚠` ambiguous / straddles, `✗` blocker.

| Module | Proposed crate | Notes |
|---|---|---|
| `core/error` | `agent-core` ✓ | Pure thiserror types. |
| `core/logging` | `agent-core` ✓ | 27 LOC tracing setup. |
| `core/models` | `agent-core` ✓ | Model catalog / context windows. |
| `core/protocol` | `agent-core` ✓ | WS `ClientMessage`/`ServerMessage` serde. |
| `core/rpc_protocol` | `agent-core` ✓ | Stdin/stdout RPC types. |
| `core/watcher_types` | `agent-core` ✓ | Pure serde — handoff/agent stats. |
| `core/chain` | `agent-core` ✓ | Session-chain ancestry walk. |
| `core/session` + `core/session_index` | `agent-core` ✓ | Filesystem session records. |
| `core/auth/` | `agent-core` ✓ | OAuth tokens; uses reqwest in `callback.rs` — **reqwest must live in core** (or split out an `agent-auth` sub-tier). |
| `core/config` | `agent-core` ⚠ | **Back-edge: depends on `tools::shell::config::ShellConfig`** (config.rs:4). See §3.A. |
| `core/compaction` | `agent-core` ⚠ | **Back-edge: depends on `runtime::Runtime`** (compaction.rs:9). See §3.B. |
| `core/rpc_dispatch` | `agent-providers` or bin ⚠ | Dispatches into runtime+tools; not a leaf type module despite living in `core/`. |
| `pricing.rs` | `agent-core` ✓ | Leaf. |
| `memory/` | `agent-core` ✓ | Genuine leaf — no `crate::` imports outside itself. |
| `events/types`, `events/queue`, `events/format` | `agent-core` ✓ | Only depend on `core/`. Move types here so providers/tools can share them. |
| `events/socket`, `events/ingest`, `events/registry` | `agent-providers` or new tier ⚠ | Need tokio/socket runtime; consumed by both providers and bin. |
| `runtime/` | `agent-providers` ✗ | See §3.C — heavy upward deps into `extensions/`, `tools/`, `events/`. |
| `runtime/openai/` | `agent-providers` ⚠ | Depends on `extensions::manager::ExtensionManager`, `extensions::providers::ProviderRegistry`, `extensions::runtime::process` (openai/mod.rs:20-22). |
| `tools/` (core trait + built-ins) | `agent-providers` or `agent-services` ⚠ | Built-ins are network/FS — fit "providers" loosely. But `tools` imports `runtime::subagent` (§3.D). |
| `tools/shell/config.rs` | `agent-core` ⚠ | Just a struct — should move down so `core/config` stops reaching up. |
| `extensions/` | `agent-services` (new) or `agent-providers` ⚠ | Bidirectional with `runtime/` (§3.E). Hooks/permissions/manifest are leafy; `runtime/process.rs` is not. |
| `extensions/hooks/`, `extensions/manifest`, `extensions/permissions`, `extensions/capability`, `extensions/validation`, `extensions/audit`, `extensions/trust` | `agent-core` ⚠ | Pure types/serde + filesystem — could be hoisted down to break the cycle. |
| `extensions/runtime/process.rs` | bin or services ✗ | 2 473 LOC; back-edge into `runtime::*` and `tools::*`. The single biggest tangle (§3.E). |
| `skills/` | bin (or `agent-services`) ✓ | No one in `runtime/`/`core/` imports `skills`. Engine + tui consume it. **Only library file importing `crossterm`** (`skills/keybinds.rs:6`). |
| `mcp/` | bin / services ✓ | Depends only on `tools::ToolRegistry`. Pure downstream. |
| `sidecar/` | bin / services ⚠ | Pulls `skills::{manifest, loader, Plugin}` (discovery.rs:17-103). Bin-side glue. |
| `engine/` | bin (`agent-runtime`) ✓ | The "boot the world" glue. Naturally bin. |
| `help.rs` | `agent-tui` or bin ⚠ | Pure model (no ratatui), but only the TUI uses `HelpFindState`. |
| `toast.rs` | `agent-tui` or bin ⚠ | Trivial; consumed by TUI. |
| `tui/` | `agent-tui` ✓ | Already isolated — no lib module imports it. |
| `cmd/`, `watcher/`, `main.rs` | `agent-runtime` (bin) ✓ | Already bin-only. |

**Ambiguity summary.** The plan's four buckets do not cover `tools/`, `extensions/`, `skills/`, `mcp/`, `events/`, `sidecar/`. These collectively are ~26 500 LOC of "service" code that is neither "types/config" (core) nor "HTTP/SSE provider" (providers) nor "rendering" (tui). The realistic split needs **either** (a) a fifth crate `agent-services` for these, **or** (b) absorb them into the bin (defeats incrementality — they get recompiled on bin edits), **or** (c) widen the definition of `agent-providers` to "everything below the TUI". Option (c) gives the best build-time win but mismatches the name.

---

## 3. Circular Dependency Hunt (THE CRITICAL PART)

Every back-edge found by grepping `crate::` paths in the library tree.

### Back-edges from intended-lower crates into intended-higher ones

#### §3.A — `core/config` → `tools/shell` (1 site)

```
src/core/config.rs:4   use crate::tools::shell::config::ShellConfig;
```

Used at config.rs:207, 238, 306-307: `SynapsConfig` embeds `ShellConfig` and the `shell.*` config keys parse straight into it.

**Break strategy.** Move `tools/shell/config.rs` (40 LOC, pure struct, no deps) **down into `agent-core`** as `core/shell_config.rs` (or `core::config::ShellConfig`). `tools::shell` then re-exports or imports it from core. Zero behavioural change. **Cost: trivial.**

#### §3.B — `core/compaction` → `runtime::Runtime` (1 site)

```
src/core/compaction.rs:9    use crate::runtime::Runtime;
src/core/compaction.rs:105  pub async fn compact_conversation(runtime: &Runtime, ...) -> Result<String>
```

This is the *only* function in the file and it makes an API call through `Runtime`. It's mis-filed — it's a **provider-level operation**, not a core type.

**Break strategy.** Move `core/compaction.rs` (227 LOC) **up into `agent-providers`** as `runtime/compaction.rs` (or `providers::compaction`). The const `COMPACTION_SYSTEM_PROMPT` plus `SUMMARIZATION_PROMPT` can stay in core if anyone other than the runtime needs them (currently no one does — only callers are `tui/commands.rs` and `engine/`, both above providers). **Cost: trivial — just a file move and one re-export.**

#### §3.C — `runtime/` reaches into `extensions/`, `tools/`, `events/`

This is the heavy back-edge cluster. Sample of the worst offenders (full list in §3.G):

```
src/runtime/mod.rs:43       hook_bus: &Arc<crate::extensions::hooks::HookBus>
src/runtime/mod.rs:158      event_queue: Arc<crate::events::EventQueue>
src/runtime/mod.rs:186      session_manager: std::sync::Arc<crate::tools::shell::SessionManager>
src/runtime/mod.rs:188      hook_bus: Arc<crate::extensions::hooks::HookBus>
src/runtime/mod.rs:207-214  crate::tools::shell::{ShellConfig, SessionManager, session::start_reaper}
src/runtime/mod.rs:231      crate::events::EventQueue::new(1000)
src/runtime/mod.rs:245      crate::extensions::hooks::HookBus::new()
src/runtime/mod.rs:521-535  crate::tools::{ToolChannels, ToolCapabilities, ToolLimits}
src/runtime/mod.rs:726      secret_prompt: Option<crate::tools::SecretPromptHandle>
src/runtime/stream.rs:11    use crate::extensions::hooks::events::HookEvent;
src/runtime/stream.rs:40-44 Arc<crate::tools::shell::SessionManager>, Arc<crate::events::EventQueue>,
                            Arc<crate::extensions::hooks::HookBus>, crate::tools::SecretPromptHandle
src/runtime/stream.rs:143-144 HookEvent::before_message + emit
src/runtime/stream.rs:289-291 / 404-406  ToolChannels/Capabilities/Limits construction
src/runtime/openai/mod.rs:20    use crate::extensions::manager::ExtensionManager;
src/runtime/openai/mod.rs:21    use crate::extensions::providers::ProviderRegistry;
src/runtime/openai/mod.rs:22    use crate::tools::{ToolCapabilities, ToolChannels, ToolContext, ToolLimits};
src/runtime/openai/mod.rs:130-164  extensions::trust + extensions::audit
src/runtime/openai/mod.rs:180-248  extensions::runtime::process::{Provider*, complete_provider_with_tools}
src/runtime/openai/mod.rs:362-363  ExtensionManager::new(HookBus::new())
```

`Runtime` literally **owns** an `Arc<HookBus>`, an `Arc<EventQueue>`, an `Arc<SessionManager>`, an `Arc<SubagentRegistry>`. The provider engine is not a leaf — it has been wired as the *system orchestrator*.

**Break strategy (layered, no single hammer):**

1. **Move `tools::ToolChannels`/`ToolCapabilities`/`ToolLimits`/`ToolContext`/`Tool` trait + `SubagentRegistry`/`SecretPromptHandle` → `agent-core`** as a `runtime_facade` module. These are vocabulary types; they don't need to live with the built-in tool implementations.
2. **Move `tools::shell::config::ShellConfig` → `agent-core`** (already required by §3.A).
3. **Move `events::queue::EventQueue` + `events::types::*` → `agent-core`** (zero deps already, see grep at top — events depends only on core).
4. **Introduce a `HookBus` trait in `agent-core`** with `async fn emit(&self, &HookEvent) -> HookResult`. The current concrete `extensions::hooks::HookBus` becomes the implementation in `agent-services`/`extensions`. `Runtime` holds `Arc<dyn HookBus>`. Same for `ExtensionManager` (use a `ProviderRouter` trait).
5. The free functions `emit_before_tool_call`, `resolve_before_tool_call_decision`, `emit_after_tool_call`, `BeforeToolCallDecision` currently in `runtime/mod.rs:35-148` become **trait-default methods or move to `agent-core`** alongside `HookEvent`/`HookResult`.
6. Only after (1)-(5) can `agent-providers` actually compile depending solely on `agent-core` + reqwest/SSE.

`Runtime::shell` and `SessionManager` are *not* really a provider concern — but extracting them is invasive. Acceptable interim: keep `agent-services::tools::shell` and let `agent-providers` depend on it (one-way), or invert by making `Runtime` accept an `Arc<dyn ShellService>` trait object.

#### §3.D — `tools/` → `runtime::subagent` (5 sites, reciprocal back-edge)

```
src/tools/mod.rs:41           pub use crate::runtime::subagent::{SubagentResult, SubagentHandle,
                                                                  SubagentRegistry, SubagentStatus,
                                                                  SubagentState};
src/tools/subagent/start.rs:15,163   crate::runtime::subagent::{...},
                                      crate::runtime::openai::extension_manager_for_routing
src/tools/subagent/resume.rs:15      crate::runtime::subagent::{...}
src/tools/subagent/steer.rs:13       crate::runtime::subagent::SubagentStatus
src/tools/subagent/collect.rs:12     crate::runtime::subagent::SubagentStatus
src/tools/subagent/oneshot.rs:6      pub use crate::runtime::subagent::SubagentResult
```

`tools/` re-exports types from `runtime/`. Combined with §3.C (`runtime/` importing `crate::tools::*`), this is a **direct mutual dependency** at the module level — would translate to a Cargo cycle the second they live in different crates.

**Break strategy.** Move `runtime/subagent.rs` (424 LOC) **down to `agent-core`** as `core::subagent`. It defines `SubagentResult`, `SubagentHandle`, `SubagentRegistry`, `SubagentStatus`, `SubagentState` — all vocabulary types and a registry, no HTTP. **Cost: low** — only `runtime/openai/extension_manager_for_routing` is still up-stack, and that's already a global function used by `tools/subagent/start.rs:163` which should move with subagent execution into providers anyway.

`tools/subagent/start.rs:163` referencing `runtime::openai::extension_manager_for_routing` is a separate back-edge from `tools → runtime`. The right fix is to thread the extension router into `ToolContext` (it's already partially there via `ToolCapabilities`), so the tool doesn't reach back into the provider crate via a global.

#### §3.E — `extensions/` ↔ `runtime/` mutual dependency (the worst single tangle)

```
src/extensions/runtime/process.rs:241   crate::runtime::resolve_before_tool_call_decision
src/extensions/runtime/process.rs:243   crate::runtime::emit_before_tool_call
src/extensions/runtime/process.rs:253-254  crate::runtime::BeforeToolCallDecision::{Continue,Block}
src/extensions/runtime/process.rs:268   crate::runtime::emit_after_tool_call
```

…combined with `runtime/mod.rs:43-148` calling **into** `crate::extensions::hooks::*`. **Runtime and extensions form a literal A↔B cycle.**

The cycle exists because the hook dispatch logic was placed in `runtime/mod.rs` (functions `emit_before_tool_call`, `resolve_before_tool_call_decision`, `BeforeToolCallDecision`, `emit_after_tool_call`) while the data types they operate on live in `extensions/hooks/events.rs` (`HookEvent`, `HookResult`, `HookBus`).

**Break strategy.**
- Move `HookEvent`, `HookResult`, the `HookBus` *trait* (split out from the current concrete struct) into `agent-core::hooks` (~577 LOC of `extensions/hooks/events.rs` is data + result enum — looks suitable for core).
- Move `BeforeToolCallDecision` and the four `emit_*`/`resolve_*` helper fns from `runtime/mod.rs:35-148` **down to `agent-core::hooks`** as well. They are pure orchestration over the trait.
- The dispatcher (subscriber registry + filtering) stays in `agent-services::extensions::hooks` as `HookBus` impl.
- Result: `runtime/` no longer mentions `extensions::*` at all; `extensions/runtime/process.rs` calls into `agent-core::hooks::*` for the helpers it currently borrows from `crate::runtime::`.

**Cost: medium** — requires moving ~600 LOC and probably re-shaping `HookBus` into a trait. But this is the single highest-value cycle break: it dissolves the runtime↔extensions back-edge in both directions in one move.

#### §3.F — `extensions/` → `sidecar/`, `memory/`, `skills/`, `tools/`

```
src/extensions/manager.rs:28-32        crate::skills::state::PluginsState
src/extensions/manager.rs:300          crate::tools::ExtensionTool::new
src/extensions/manager.rs:564          crate::sidecar::spawn::SidecarSpawnArgs
src/extensions/manifest.rs:32,39       references crate::skills::post_install (docs only)
src/extensions/runtime/mod.rs:132      Result<crate::sidecar::spawn::SidecarSpawnArgs, String>
src/extensions/runtime/process.rs:981  use crate::memory::store::{self, MemoryQuery};
src/extensions/runtime/process.rs:1895 Result<crate::sidecar::spawn::SidecarSpawnArgs, String>
```

These are forward dependencies (downward in the proposed graph if `extensions` lives *above* sidecar/memory/skills/tools). They're not cycles per se, but they pin `extensions/` to bin-side glue (`skills`, `sidecar`) — so `extensions/` cannot sit cleanly in `agent-providers`. It needs to be in `agent-services` or the bin.

#### §3.G — Complete back-edge tally (lib-only, sorted by severity)

| Edge (from → to) | Sites | Severity | Reference |
|---|---:|---|---|
| `runtime` → `extensions` | ~25 | **Cycle w/ §3.E** | runtime/mod.rs:43-156, runtime/stream.rs:11-144, runtime/openai/mod.rs:20-363 |
| `extensions` → `runtime` | 5 | **Cycle w/ §3.C** | extensions/runtime/process.rs:241-268 |
| `tools` → `runtime` | 6 | **Cycle w/ §3.C** | tools/mod.rs:41, tools/subagent/{start,resume,steer,collect,oneshot}.rs |
| `runtime` → `tools` | 14 | High | runtime/mod.rs:62-870, runtime/stream.rs:40-406, runtime/openai/mod.rs:22 |
| `runtime` → `events` | 4 | High | runtime/mod.rs:158, 231, 283; runtime/stream.rs:42 |
| `tools` → `events` | 1 | Medium | tools/mod.rs:69 (`event_queue: Option<Arc<crate::events::EventQueue>>`) |
| `tools` → `extensions` | 2 | High | tools/extension.rs:6-7 (`ExtensionHandler`, `RegisteredExtensionToolSpec`) |
| `core` → `runtime` | 1 | **Cycle if `runtime` ever needs `core` (it does)** | core/compaction.rs:9 — see §3.B |
| `core` → `tools` | 1 | **Cycle** | core/config.rs:4 — see §3.A |
| `extensions` → `tools` | 1 | Medium | extensions/manager.rs:300 |
| `extensions` → `sidecar` | 3 | Low | extensions/manager.rs:564, extensions/runtime/{mod,process}.rs |
| `extensions` → `skills` | 3 | Low | extensions/manager.rs:28-32 |
| `extensions` → `memory` | 1 | Low | extensions/runtime/process.rs:981 |
| `skills` → `extensions` | 2 | Low | skills/manifest.rs:79, skills/trust.rs:111 |
| `skills` → `tools` | 1 | Medium | skills/commands.rs:10-11 |
| `mcp` → `tools` | 1 | Low | mcp/mod.rs:9 (`use crate::ToolRegistry`) |
| `sidecar` → `skills` | 3 | Low | sidecar/discovery.rs:17-103 |
| `engine` → `runtime`/`skills`/`mcp`/`extensions`/`events` | many | (engine = glue; lives in bin anyway) | engine/setup.rs |
| `help.rs` → `skills` | 1 | Low | help.rs:747 |

**Lib → tui edges:** **none.** Verified by `grep -rE 'crate::tui|synaps_cli::tui'` across all library directories — zero hits outside the bin tree. The TUI separation is genuinely clean.

**Lib → cmd / watcher edges:** **none.**

### Shared-state chokepoints crossing proposed boundaries

The S205 review's claim of "98 `Arc<Mutex>` + 158 channels" is in the right ballpark — this recon counted **67** `Arc<Lock>` instances and **164** channel mentions across `src/`. The ones that will straddle the split:

| Shared handle | Owner | Crossed by |
|---|---|---|
| `Arc<extensions::hooks::HookBus>` | `Runtime` field (runtime/mod.rs:188) | core ↔ providers ↔ services |
| `Arc<events::EventQueue>` | `Runtime` field (runtime/mod.rs:158) | providers + tools + bin |
| `Arc<tools::shell::SessionManager>` | `Runtime` field (runtime/mod.rs:186) | providers ↔ tools |
| `Arc<Mutex<SubagentRegistry>>` | `Runtime` field (runtime/mod.rs:156) **but type lives in runtime/subagent.rs** | providers ↔ tools (§3.D) |
| `Arc<RwLock<ToolRegistry>>` | `Runtime` field | providers ↔ tools |
| `Arc<RwLock<ExtensionManager>>` | global (`runtime/openai/mod.rs:32-41`) | providers ↔ services |
| `SecretPromptHandle` (`UnboundedSender<SecretPromptRequest>`) | flows runtime → tool ctx → TUI | providers → tui |
| `tx_delta`/`tx_events` mpsc | created in runtime, consumed by tools and TUI | providers → tui |

Each is a candidate trait object after §3.C/§3.E surgery.

---

## 4. Shared-State Chokepoints

### The `App` struct (`src/tui/app.rs:43`)

`App` is a ~120-field god-struct (lines 43-160+) holding:
- chat history (`messages`, `api_messages`, `line_cache`)
- input state (`input`, `cursor_pos`, `tab_cycle`, history)
- session metadata (`session`, `agent_name`, token counters)
- UI sub-state (`settings`, `plugins`, `models`, `help_find`, `subagents`)
- background-task handles (`compact_task: tokio::task::JoinHandle<…>`, `gamba_child: std::process::Child`)
- async channels (`ping_tx/rx`, `model_list_tx/rx`)
- terminal coordinates (`selection_anchor`, `msg_area_rect`, `visible_line_range`)

**Who depends on it:** only `tui/*.rs` files. Confirmed by `grep -rnE 'crate::tui|synaps_cli::tui'` — zero hits outside `src/tui/` and `src/main.rs`. **No core/providers/services code touches `App`.** This is the single best piece of news for the split.

**Belongs in:** `agent-tui` (or in the bin). It carries `Session`, `Runtime` handles, `Result<…RuntimeError>` from the lib — those imports stay one-directional.

### `Runtime` (`src/runtime/mod.rs:141`)

The "engine" of the system. Owns reqwest client, auth state, tool registry, subagent registry, event queue, hook bus, shell session manager, telemetry. **Cannot move to `agent-providers` until §3.C and §3.E are resolved** — currently has 5 distinct `crate::extensions::*`, `crate::tools::*`, `crate::events::*` references in its struct definition (lines 156-188) and ~30 more in its method bodies.

**Pre-conditions for `Runtime` to live in `agent-providers`:**

1. `HookBus` becomes `Arc<dyn HookBus>` from `agent-core` (§3.E).
2. `EventQueue` moves to `agent-core` (it has zero outbound deps).
3. `SessionManager` (shell PTY) either moves with `tools::shell` into `agent-services` and is injected as `Arc<dyn ShellService>`, **or** the shell PTY code (~1.5k LOC, see `tools/shell/`) moves into `agent-providers` directly.
4. `SubagentRegistry` moves to `agent-core` (§3.D).
5. `ToolRegistry` either moves to `agent-core` (with the `Tool` trait) or `Runtime` holds it as a trait object.

### `runtime/` module ownership

`runtime/openai/` uses `set_extension_manager_for_routing` (mod.rs:32-41) — a **static global** `OnceLock<Arc<RwLock<ExtensionManager>>>`. This is set in `engine/setup.rs:183` and read in `tools/subagent/start.rs:163`. It crosses every proposed boundary. After the split, this should become an explicit parameter on the relevant call sites, not a global.

### `extensions/`

Owns plugin lifecycle, JSON-RPC process bridge, hook subscription registry. Belongs in `agent-services` (new crate) or the bin. Cannot be in `agent-providers` because (a) it imports `sidecar::spawn`, `skills::state`, `memory::store`, and (b) `runtime/` depends on its public surface — if you put both in providers you keep the cycle.

### `skills/`

Pure plugin discovery + slash-command registry. Imports nothing in `runtime/`. Imports one type from `extensions::manifest` (manifest.rs:79). Imports `tools::{ToolCapabilities, ToolChannels, ToolLimits}` for `commands.rs:10`. Only library file using `crossterm` (`keybinds.rs:6`) — that's fine if it lives in the bin or in `agent-tui`. **Recommend: bin or new `agent-services`.**

---

## 5. Cut Order (Safe Sequence)

Each step is designed to leave `cargo check` green. **No Cargo.toml work in steps 0-3** — those are in-repo refactors against the existing single crate.

### Step 0 — Land trivial back-edge fixes (still single crate)

These don't require touching Cargo.toml; they merely re-arrange so future Cargo surgery is possible.

| Action | File(s) | Why |
|---|---|---|
| Move `tools/shell/config.rs` → `core/shell_config.rs`; re-export from `tools::shell` | core/config.rs:4 | Cuts §3.A |
| Move `core/compaction.rs` → `runtime/compaction.rs` | core/compaction.rs:9, callers at tui/commands.rs and engine/ | Cuts §3.B |
| Move `runtime/subagent.rs` → `core/subagent.rs` (re-export from runtime + tools) | runtime/mod.rs:18, tools/mod.rs:41 | Cuts §3.D |
| Move `events/types.rs` + `events/queue.rs` + `events/format.rs` → under `core::events` | events/mod.rs | Lets providers/tools share without depending on a sibling crate |

Test after each: `cargo check` and `cargo test --no-run`.

### Step 1 — Hook-cycle break (still single crate)

The single highest-value change. Cuts §3.C/§3.E in both directions.

| Action | Detail |
|---|---|
| Define `pub trait HookBus { async fn emit(&self, &HookEvent) -> HookResult; }` in a new `core::hooks` module | Move `HookEvent`, `HookResult` out of `extensions/hooks/events.rs` into `core::hooks::events` |
| Move `BeforeToolCallDecision`, `emit_before_tool_call`, `resolve_before_tool_call_decision`, `emit_after_tool_call` from `runtime/mod.rs:35-148` → `core::hooks` | These are pure trait-driven helpers |
| Convert the existing `extensions::hooks::HookBus` struct into an `impl HookBus for ConcreteHookBus` | Same allocations, same API |
| Change `Runtime` to hold `Arc<dyn HookBus>` instead of `Arc<extensions::hooks::HookBus>` | runtime/mod.rs:188, runtime/stream.rs:43 |

After step 1, **`grep -rE 'crate::extensions' src/runtime` should return zero (or near-zero) hits** outside `runtime/openai/mod.rs`. Verify before continuing.

### Step 2 — Trait-erase the remaining provider→services edges (still single crate)

| Action | Detail |
|---|---|
| Move `tools::{Tool, ToolContext, ToolChannels, ToolCapabilities, ToolLimits, ToolRegistry, SecretPromptHandle, SecretPromptRequest}` → `core::tool_facade` | These are vocabulary types, ~200 LOC total in `tools/mod.rs` head |
| Replace `set_extension_manager_for_routing` global (`runtime/openai/mod.rs:32-41`) with explicit param threaded through `ApiOptions` / `ToolContext` | Kills the hidden global crossing all crate boundaries |
| Move `extensions::runtime::process::{ProviderCompleteParams, ProviderCompleteResult, ProviderStreamEvent, NotificationFrame, complete_provider_with_tools}` signature surface into a trait `ProviderEngine` in `core` | `runtime/openai/mod.rs:180-248` becomes a trait dispatch |

Goal at end of step 2: `runtime/` imports only `core::*` and external crates (reqwest, tokio, etc.). Verify with `grep -rE 'crate::(extensions|tools|events|skills|mcp|memory|sidecar)' src/runtime`.

### Step 3 — Extract `agent-core` (FIRST Cargo.toml surgery)

Now we touch Cargo.toml. Create a workspace.

```
agent-runtime/                 (workspace root)
├── Cargo.toml                 (workspace = ["crates/*"], default-members = ["crates/agent-runtime"])
├── crates/
│   ├── agent-core/           ← Step 3 extracts this
│   ├── agent-providers/      ← Step 4
│   ├── agent-services/       ← Step 5 (the "5th bucket")
│   ├── agent-tui/            ← Step 6
│   └── agent-runtime/        ← Step 7 (bin, currently src/)
```

Contents of `agent-core` (proposed):

```
core/{error,logging,models,protocol,rpc_protocol,session,session_index,
       chain,watcher_types,config,shell_config,subagent,hooks/{events,trait},
       tool_facade}
events/{types,queue,format}
memory/                       (genuine leaf)
pricing.rs
core/auth/                    (carries reqwest dep — acceptable for core, or extract agent-auth)
```

Path: move files, update `Cargo.toml` of root to expose only `agent-core`, switch `synaps_cli` lib references in non-extracted code to `agent_core::`. Verify `cargo check -p agent-core` is independent of the rest.

### Step 4 — Extract `agent-providers`

Contents:
```
runtime/{mod,api,api_sync,auth,helpers,request,sse,sse_types,stream,
         compaction,telemetry,types,openai/*}
```

Deps: `agent-core` + reqwest/tokio/futures/serde_json. **Must not depend on `agent-services` or `agent-tui`.** Verify with `cargo check -p agent-providers`.

### Step 5 — Extract `agent-services`

Contents:
```
tools/                        (built-ins + shell PTY)
extensions/                   (manager, hooks dispatcher impl, process, manifest, ...)
skills/                       (plugin/skill discovery + registry — also crossterm dep)
mcp/
sidecar/
engine/                       (the boot orchestrator — could equally go in bin)
help.rs
toast.rs
```

Deps: `agent-core` + `agent-providers`. This crate is where all the "service" code lives. ~26 500 LOC.

⚠ **Decision point.** If `agent-services` and `agent-tui` rarely change together, this split is worth it. If they change together (likely true for `extensions/widgets`, `skills/keybinds`, `skills/commands` which TUI consumes heavily), combine them — see §6 risk R3.

### Step 6 — Extract `agent-tui`

Contents: `tui/`.
Deps: `agent-core` + `agent-services` + `agent-providers` (for `Runtime`, `StreamEvent`) + ratatui/crossterm/syntect/tachyonfx.

### Step 7 — `agent-runtime` becomes the bin crate

Contents: `main.rs`, `cmd/`, `watcher/`, `bin/hidden/`.
Deps: `agent-core` + `agent-providers` + `agent-services` + `agent-tui`. ~9 500 LOC of glue.

### Modules that get split across crates

| Module | Split |
|---|---|
| `core/config.rs` | Stays in `agent-core` after Step 0 (ShellConfig moved down) |
| `events/` | `{types,queue,format}` to core; `{socket,ingest,registry}` to services |
| `extensions/hooks/` | Types + trait to core; dispatcher impl + subscription registry to services |
| `tools/` | Trait + context types to core; built-ins to services |
| `runtime/subagent.rs` | Types to core; execution glue (if any) to providers |
| `runtime/openai/` | All in providers, but the **`ExtensionManager` global** dies (Step 2) |

---

## 6. Risk Register

Ranked by `Likelihood × Impact`.

### R1 — The `runtime ↔ extensions ↔ tools` cycle is not a "back-edge" — it's a load-bearing wall. **HIGH / CRITICAL**

If anyone attempts the Cargo split before Steps 1-2 (the trait-extraction passes) land, the workspace will not compile. There is **no Cargo.toml surgery that resolves this** — it must be done in source first.

*Mitigation:* enforce the cut order above. After Step 1, add a CI grep gate: `! grep -rE 'crate::extensions' src/runtime/` must pass.

### R2 — `Runtime` holds 5+ pieces of shared mutable state that all need trait-erasure together. **HIGH / HIGH**

Touching one (e.g. `HookBus`) without touching the others (`EventQueue`, `SessionManager`, `SubagentRegistry`, `ToolRegistry`, `ExtensionManager`) produces an inconsistent design where some shared state is trait-erased and some is concrete. Risk of half-finished refactor sitting on `main`.

*Mitigation:* land Steps 1-2 in a single feature branch, with a checklist. Don't merge partial state.

### R3 — `agent-services` may not actually buy build-time wins. **MEDIUM / HIGH**

The brief's "~25k recompile not ~75k" target assumes TUI edits don't touch services. But `skills/keybinds.rs`, `skills/commands.rs`, `extensions/widgets.rs`, `extensions/settings_editor.rs` are co-edited with TUI changes. If services + tui change together, services recompiles before tui every time and you save little.

*Mitigation:* measure first. After Step 5, run `cargo check` after a representative TUI edit (e.g. modify `tui/draw.rs`) and confirm services does **not** rebuild. If it does, consider folding services back into `agent-tui` or `agent-runtime` (bin).

### R4 — `core/auth/callback.rs` pulls reqwest. **MEDIUM / MEDIUM**

`agent-core` is intended as "types, config, errors, serde". Carrying reqwest contradicts that.

*Mitigation:* either (a) accept that auth uses reqwest (it's small, well-isolated), or (b) split out `agent-auth` as a sibling of core. Recommend (a) — the OAuth callback server is ~100 LOC and isolates cleanly.

### R5 — The `extension_manager_for_routing` global (`runtime/openai/mod.rs:32-41`). **MEDIUM / HIGH**

It's a `OnceLock` set in `engine/setup.rs:183`, read in `tools/subagent/start.rs:163`. It crosses every proposed crate boundary at runtime. If you keep it as a global, you've kept the cycle — it just hides at the symbol level.

*Mitigation:* delete the global in Step 2. Thread `Arc<dyn ProviderRouter>` through `ToolContext::capabilities` instead.

### R6 — `engine/` placement is ambiguous. **MEDIUM / MEDIUM**

`engine/setup.rs` imports `crate::{skills, mcp, events, extensions, runtime, tools}`. It's the system boot. It feels like services, but if TUI imports it (it does: `tui/mod.rs:30` `synaps_cli::engine::setup::boot`), engine has to live below TUI. Putting it in `agent-services` is fine, but it makes services a "must-recompile on engine touch" dep of TUI.

*Mitigation:* move `engine/` into the bin (`agent-runtime`). Both `tui/run` and `cmd/chat::run` are already bin-level. The `engine` module is glue for them — it belongs with them.

### R7 — `cmd/server.rs` (1 058 LOC, axum WS server) and `cmd/rpc.rs` (801 LOC, JSON-RPC) significantly expand the bin's dep surface (axum, tower, tokio-tungstenite). **LOW / MEDIUM**

These are bin-only today and stay bin-only after split — but they may benefit from their own crate down the line.

*Mitigation:* none for A3. Note for a future A4.

### R8 — `bin/hidden/` is a separate binary (6 455 LOC) that may or may not survive the split intact. **LOW / LOW**

Not on the A3 critical path. Excluded from this recon.

*Mitigation:* verify it still compiles after Step 7 by running `cargo build --bin hidden`.

### R9 — Test fixtures and integration tests under `tests/` may reference `crate::` paths that move. **LOW / MEDIUM**

Not surveyed in this recon (scoped to `src/`).

*Mitigation:* after Step 0, run full `cargo test --no-run` and triage compile errors.

### R10 — `cmd/server.rs` and `cmd/rpc.rs` may hold their own `Arc<Mutex>` chokepoints not visible from this recon. **LOW / MEDIUM**

Not surveyed.

*Mitigation:* spot-check before Step 7. Out of scope for the foundational split.

---

## Appendix A — Methodology

- `grep -rnE '^use crate::' src/<module>/` per module to enumerate inbound deps.
- `grep -rnE 'crate::<other>' src/<module>/` to catch fully-qualified path uses not at top-of-file.
- Verified no library module touches `crate::tui` (zero hits).
- Verified library-side `crossterm` use is confined to `skills/keybinds.rs:6`.
- LOC via `wc -l`; Cargo.toml read but not modified.
- `target/` excluded from all searches.
- No code generation, no `cargo expand`, no compilation attempted.

## Appendix B — Files Worth Reading Before Step 1

For anyone implementing the cycle break:

- `src/extensions/hooks/events.rs` (577 LOC) — `HookEvent`, `HookResult` definitions. Will mostly move to `core::hooks::events`.
- `src/extensions/hooks/mod.rs` (745 LOC) — `HookBus` impl. The trait extracts cleanly from the top; the subscription registry stays.
- `src/runtime/mod.rs:35-148` — the four free fns that move to `core::hooks`.
- `src/runtime/mod.rs:141-250` — the `Runtime` struct definition. Every field is a candidate for trait-erasure.
- `src/runtime/openai/mod.rs:1-260` — the OpenAI provider engine and its `extensions::*` reach-ups.
- `src/extensions/runtime/process.rs:230-280` — the reciprocal `crate::runtime::*` calls.
- `src/tools/mod.rs:1-80` — the `Tool` trait + `ToolContext`/`Capabilities`/`Channels`/`Limits` types that should move down.

---

*End of report.*
