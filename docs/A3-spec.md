---
title: "A3 Spec — 3-Crate Workspace Split"
created: "2026-06-13"
session: S208
branch: refactor/a3-crate-split
supersedes: "docs/REVIEW-S205.md A3 (4-crate), docs/A3-recon.md 5-crate option"
relates: ["#117", "#116"]
---

# A3 Spec — 3-Crate Split (core / engine / tui + bin)

## Decision

**3 crates, not 5.** The `runtime ↔ extensions ↔ tools` cycle is only a blocker if
`providers` and `services` are split into separate crates. Folding them into one
`agent-engine` crate keeps the cycle **internal to a single crate (legal in Rust)** and
**eliminates the high-risk trait-erasure surgery** entirely.

Build-speed win comes from isolating the layer we edit most (TUI) into the top crate.
`agent-tui` boundary is VERIFIED clean (zero modules reach into `crate::tui`). That cut
captures ~80% of the win at ~Step-0 risk, and gives #116 its compiler-enforced boundary.

## Target Layout — clean DAG, no cycles across edges

```
agent-runtime (bin)   main.rs, cmd/, watcher/, bin/hidden/, engine/(boot orchestrator)
      │
      ▼
agent-tui             tui/                          (ratatui, crossterm, syntect, tachyonfx)
      │
      ▼
agent-engine          runtime/ tools/ extensions/   (reqwest, SSE, PTY, MCP, skills)
      │               skills/ mcp/ sidecar/ events/
      │               ← the runtime↔extensions↔tools cycle lives HERE, internal & fine
      ▼
agent-core            error, logging, models, protocol, rpc_protocol, session,
                      session_index, chain, watcher_types, config, shell_config,
                      memory/, pricing.rs, auth/   (leaf — depends on NOTHING in-repo)
```

Dependency rule (enforce after each step):
- `agent-core` imports **no** `crate::{runtime,tools,extensions,skills,mcp,sidecar,events,engine,tui}`
- `agent-engine` imports `agent-core` + external only — **never** `tui` or bin
- `agent-tui` imports `agent-core` + `agent-engine` + render deps — never bin
- bin imports all three

## Step 0 — Make `agent-core` a clean leaf (single crate, no Cargo.toml yet)

Only TWO genuine back-edges block core from being a leaf (per recon §3.A/§3.B):

| # | Action | Cuts |
|---|---|---|
| 1 | Move `tools/shell/config.rs` (`ShellConfig`) → `core/shell_config.rs`; re-export from `tools::shell` so existing `tools::shell::config::ShellConfig` paths still resolve | `core/config.rs:4 → crate::tools::shell::config::ShellConfig` |
| 2 | Move `core/compaction.rs` → `runtime/compaction.rs` (it's misfiled — imports `crate::runtime::Runtime`); fix callers in `tui/commands.rs` + `engine/` | `core/compaction.rs:9 → crate::runtime::Runtime` |

**Verification gate after EACH move:** `cargo check` green + `cargo test --no-run` green.
Then prove core is a leaf:
```
# from the proposed core module set, this MUST return zero hits:
grep -rnE 'crate::(runtime|tools|extensions|skills|mcp|sidecar|events|engine|tui)::' \
  src/{error,logging,models,protocol,rpc_protocol,session,session_index,chain,watcher_types,config,shell_config,compaction}.rs \
  src/memory/ src/pricing.rs src/auth/ 2>/dev/null
```
(Adjust the file list to actual core membership; the point is: zero upward imports.)

NOTE: recon's Step-0 moves for `subagent` and `events` into core are **NOT needed** here —
they existed only to let separate providers/services crates share. In the 3-crate plan
both live in `agent-engine`, so they stay put.

## Steps 1-4 (Cargo surgery, each leaves tree green)

1. **Extract `agent-core`** — workspace root `Cargo.toml` (`members = ["crates/*"]`),
   move core files to `crates/agent-core/`, switch in-repo refs to `agent_core::`.
   Verify `cargo check -p agent-core` builds independently of everything else.
2. **Extract `agent-engine`** — runtime/tools/extensions/skills/mcp/sidecar/events.
   Deps: `agent-core` + external. Verify it does NOT depend on tui/bin.
3. **Extract `agent-tui`** — `tui/`. Deps: core + engine + render crates.
4. **bin = `agent-runtime`** — main/cmd/watcher/engine-boot/hidden. Full suite green + warning-free.
5. **Benchmark** — baseline warm `cargo check` = 1m33s. After split, edit `tui/draw.rs`
   and time incremental `cargo check`. Confirm engine does NOT rebuild on a pure-TUI edit.

## Open question deferred to data
If a pure-TUI edit rebuilds `engine` too often (R3), revisit splitting `engine →
providers + services` — but only with measured justification, not on spec.
