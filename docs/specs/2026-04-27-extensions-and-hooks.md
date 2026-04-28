# Spec: Extensions & Hooks

**Status:** Draft (awaiting human review)
**Date:** 2026-04-27
**Convergence mode:** holdout (per planning skill, fixed at plan time)

---

## 1. Objective

Give SynapsCLI a first-class extension system so the binary ships with
**compiled-in extension points (hooks)** and external **extensions** —
written in any language — can subscribe to those hooks and manipulate
the app through a stable, capability-gated **Synaps API**.

The mental model is fixed:

```
SynapsCLI binary
  ├─ compiled-in hook call sites      ← part of the core, always present
  ├─ HookBus (dispatcher)              ← part of the core
  ├─ ExtensionManager / Runtimes       ← part of the core
  ├─ SynapsApi (capability surface)    ← part of the core
  └─ optional external extensions
        ├─ Process/JSON-RPC runtime    ← phase 1
        ├─ Script-hook runtime          ← phase 1.5 (post-process runtime)
        ├─ WASM runtime                 ← later epic
        └─ Native Rust runtime          ← later epic, internal only
```

**Hooks are not extensions.** Hooks are stable extension points compiled
into the binary. Extensions consume those hooks.

### Success criteria

- Compiled-in hook sites exist for the phase-1 catalog and emit events
  whether or not extensions are installed.
- Without any extensions installed, runtime overhead from the HookBus is
  negligible (<1µs per emit on a no-handler path; verified with a
  microbench).
- An extension written in TypeScript (Node) and one written in Python can
  both register against the same `before_tool_call` hook, block a
  bash command, and modify input — using only the documented protocol.
- An extension can `register_tool` and the LLM can call that tool in the
  same session, with no rebuild of SynapsCLI.
- An extension can `register_provider` and a `/model` switch routes to it.
- Permissions declared in the manifest are enforced: an extension without
  `tools.intercept` cannot read `before_tool_call` event input.
- The existing `.synaps-plugin/plugin.json` continues to work for
  skills-only plugins (backwards compatible).

### Non-goals (phase 1)

- WASM runtime
- Native dynamic-library extensions
- Hot reload of extensions
- Custom UI components / message renderers from extensions
- Compaction, fork/clone, message-routing, install-scan, model-select hooks

---

## 2. Stakes (for convergence-mode rationale)

This work is `convergence: holdout` because:

- **Blast radius:** changes the core agent loop, tool execution path,
  and provider routing. A regression breaks every session.
- **Security-critical:** extensions execute third-party code with access
  to tool input/output and (when permitted) prompt content. Permission
  enforcement bugs leak prompts, secrets, or filesystem access.
- **Bias risk:** the same author would naturally mark their own
  permission model favourably. We need an independent judge with an
  information wall.

Holdout test set (written by tester before builder sees the spec)
includes:

- Permission-bypass attempts
- Hook ordering and middleware-chain tests
- No-extensions overhead microbench
- Manifest backwards-compatibility tests (skills-only plugins)
- Privileged-event gating (LLM input/output access)

---

## 3. Commands

```bash
# Build
cargo build
cargo build --release

# Test (workspace + integration)
cargo test
cargo test --test extensions_e2e        # new
cargo test -p synaps-cli ext::          # new module tests

# Lints
cargo clippy --all-targets -- -D warnings
cargo fmt --check

# Microbench (no-extensions overhead)
cargo bench --bench hookbus_overhead    # new

# Manual smoke
./target/debug/synaps run --extension ./examples/safe-bash/manifest.json
```

---

## 4. Project structure

New top-level module: `src/extensions/`.

```
src/
├── extensions/
│   ├── mod.rs
│   ├── hooks/
│   │   ├── mod.rs              # HookBus, HookKind, HookResult
│   │   ├── events.rs           # HookEvent enum + per-event structs
│   │   └── dispatcher.rs       # ordering, middleware chaining, timeouts
│   ├── api.rs                  # SynapsApi trait + concrete impl
│   ├── context.rs              # ExtensionContext passed into handlers
│   ├── manager.rs              # discovery, lifecycle, enable/disable
│   ├── manifest.rs             # extension manifest parsing
│   ├── permissions.rs          # permission model + enforcement
│   └── runtime/
│       ├── mod.rs              # ExtensionRuntime trait
│       ├── builtin.rs          # in-process Rust extensions
│       └── process.rs          # JSON-RPC over stdio
├── runtime/
│   └── providers/              # NEW — provider abstraction
│       ├── mod.rs              # ModelProvider trait + ProviderRegistry
│       ├── anthropic.rs
│       └── openai_compat.rs    # wraps existing static specs
└── tools/
    └── registry.rs             # add register_from_extension hook

docs/
├── extensions/
│   ├── README.md               # user-facing overview
│   ├── protocol.md             # JSON-RPC protocol spec
│   ├── hooks.md                # phase-1 hook catalog
│   ├── permissions.md          # permission flags reference
│   └── examples/
│       ├── typescript-safe-bash/
│       └── python-policy/
└── specs/2026-04-27-extensions-and-hooks.md  # this file

tests/
├── extensions_e2e.rs           # end-to-end with process runtime
├── extensions_permissions.rs   # permission enforcement
└── extensions_compat.rs        # skills-only plugin still works

benches/
└── hookbus_overhead.rs         # no-handler emit cost
```

---

## 5. Code style

Match existing patterns in `src/tools/mod.rs` and `src/skills/loader.rs`:

```rust
//! Extension hook bus — compiled-in extension points.
//!
//! HookBus dispatches typed events to registered handlers. Without any
//! extensions installed, emit() is a no-op fast path.

use std::sync::Arc;
use crate::Result;

#[async_trait::async_trait]
pub trait HookHandler: Send + Sync {
    fn id(&self) -> &str;
    fn kinds(&self) -> &[HookKind];
    async fn handle(
        &self,
        event: &mut HookEvent,
        ctx: HookContext,
    ) -> Result<HookResult>;
}

#[derive(Debug, Clone)]
pub enum HookResult {
    Continue,
    Block { reason: String },
    Modify,                        // event was mutated in place
    RequireApproval { reason: String },
}
```

- All public traits derive `Send + Sync` and use `#[async_trait]`.
- Errors flow through `crate::Result` / `RuntimeError`.
- Permissions are checked **before** event delivery, not inside the handler.
- Tracing spans named `ext.hook.<kind>` and `ext.api.<method>`.

---

## 6. Testing strategy

- **Unit tests** colocated under each module's `#[cfg(test)]`.
- **Integration tests** under `tests/`, using a fixture extension binary
  (Rust test bin) that speaks the JSON-RPC protocol.
- **TS extension test** under `tests/fixtures/ts-ext/` — small Node
  extension, run only when `node` is on PATH.
- **Bench** under `benches/` using `criterion`.
- **Holdout test set** authored by an independent tester subagent
  *before* the builder sees the spec body. The holdout fixtures live
  under `tests/holdout/` and are not visible to the builder.

Coverage expectations:

- `extensions::hooks` — ≥90% line coverage
- `extensions::permissions` — 100% branch coverage on enforcement
- `extensions::runtime::process` — happy path + 5 failure modes

---

## 7. Boundaries

### Always do

- Run `cargo test`, `cargo clippy --all-targets -- -D warnings`,
  `cargo fmt --check` before any commit.
- Check permissions before delivering a hook event.
- Wrap every handler call in a per-hook timeout.
- Trace every API call with `runId`, `extensionId`, `hookKind`.
- Keep the no-extensions fast path zero-allocation where feasible.
- Update `docs/extensions/hooks.md` when adding or modifying a hook.

### Ask first

- Adding a new hook to the phase-1 catalog.
- Adding a new permission flag.
- Adding a new Synaps API method.
- Adding a new runtime kind.
- Touching the existing static `ProviderSpec` shape (it's referenced by
  the OpenAI router).
- Changing the `.synaps-plugin/plugin.json` schema.

### Never do

- Give an extension direct mutable access to internal `RuntimeSession`,
  `ToolRegistry`, `Config`, or provider clients.
- Deliver `before_provider_request` / `llm_input` / `llm_output` events
  to an extension lacking the `privacy.llm_content` permission.
- Allow an extension to register a tool with the same name as a built-in
  without `tools.override` permission.
- Block the agent loop on a misbehaving extension — every hook call has
  a hard timeout.
- Commit holdout fixtures or judge artifacts to the builder's branch
  before the loop completes.

---

## 8. Open questions for the human (block before plan approval)

1. **Process protocol:** JSON-RPC 2.0 over stdio, or LSP-style
   framed JSON? Recommendation: JSON-RPC 2.0 over stdio with
   `Content-Length:` framing (LSP-style) — well understood, easy to
   implement in any language.

2. **Extension discovery roots:** reuse the existing skill discovery
   roots (`.synaps-cli/plugins/`, `~/.synaps-cli/plugins/`) or add a
   parallel `extensions/` directory? Recommendation: reuse — extension
   capability lives inside the same `.synaps-plugin/plugin.json`.

3. **Trust model on first run:** prompt-on-first-use (per host) like
   the existing marketplace trust prompt, or explicit
   `synaps extensions trust <name>` CLI step? Recommendation:
   prompt-on-first-use, mirroring `TrustPrompt` in
   `src/chatui/plugins/state.rs`.

4. **Provider refactor scope:** refactor *all* providers to the new
   `ModelProvider` trait in phase 1, or only introduce the trait and
   migrate the OpenAI-compatible path, leaving Anthropic on the
   existing direct path? Recommendation: introduce trait + migrate
   OpenAI-compat first; Anthropic migration as a follow-on so the
   surface area stays small.

5. **Bench tool:** `criterion` is already common in the Rust
   ecosystem but not currently a dependency. OK to add as a
   `[dev-dependencies]` entry?

→ Answer these before plan tasks are finalized; they affect 3–5 task
boundaries.
