# Code Review — A3 Crate Split + #116 Render Thread

> Reviewer: **Chrollo** (architecture & crate-boundaries lens)
> Branch: `dev` @ `17051f2`  ·  Scope: read-only
> Compared against: `docs/specs/A3-spec.md`, `docs/reviews/A3-recon.md`

---

## 0. Executive verdict

The split landed cleanly. The four-node DAG holds (`agent-core ← agent-engine ← agent-tui ← bin`), the deliberate intra-`agent-engine` cycle is contained as the spec promised, and the `extern crate self as synaps_cli` trick collapses ~360 internal call sites in `agent-tui` and ~60 in `src/` without a single touch — elegant for what it bought (no call-site churn). Verified empirically by the 2.79s incremental hot-loop check.

That said, the manifest layer was not audited with the same rigor as the code layer. The most consequential finding (HIGH-1) is silent and easy to miss: **no `resolver = "2"` was declared** when the workspace was introduced. Several lesser MEDIUMs follow from the same blind spot — unused root deps, a feature mismatch on `tachyonfx`, and a wider-than-necessary `pub mod` surface on `agent-engine`. None of these block the merge; all should be cleaned up before any subcrate is published or before a contributor adds the next dep.

No CRITICAL findings. The architecture is sound.

---

## 1. The 3-crate split — boundary audit

### 1.1 DAG enforcement (verified)

| Edge | Method | Result |
|---|---|---|
| `agent-core` has no upward refs | `grep -rEn 'crate::(runtime\|tools\|extensions\|skills\|mcp\|sidecar\|events\|engine\|tui)' crates/agent-core/src` | **0 hits** ✓ |
| `agent-engine` has no `tui`/`cmd`/`watcher` refs | same grep, filtered | **0 hits** (one false positive on `crate::watcher_types` which is an engine-level alias, not the bin's `watcher`) ✓ |
| `agent-tui` has no `cmd`/`watcher`/`bin` refs | grep | **0 hits** ✓ |
| `agent-engine` ⟷ `agent-tui` references in either direction | `synaps_cli::` in engine, `agent_tui::` in engine, `crate::tui::` in engine | **0 hits** ✓ |

DAG is intact. The recon's R3 hypothesis is empirically refuted in `A3-spec.md` Results — incremental hot-loop confirmed.

### 1.2 The internal `runtime ↔ extensions ↔ tools` cycle (acceptable)

Spec calls this out explicitly and accepts it. Spot-check confirms it stays inside `crates/agent-engine/src/`:

- `crates/agent-engine/src/tools/extension.rs:6-7` → `use crate::extensions::runtime::ExtensionHandler;`
- `crates/agent-engine/src/tools/subagent/start.rs:15` → `use crate::runtime::subagent::{SubagentHandle, …};`
- `crates/agent-engine/src/extensions/runtime/process.rs` — 13 cross-module pulls; the densest knot, but all within engine.
- `crates/agent-engine/src/runtime/openai/mod.rs` — 4 cross-module pulls.

**Verdict:** The cycle is real but legal (intra-crate). The decision to fold `providers`+`services` into one engine crate to absorb it was the correct trade — the alternative (trait-erasure surgery) is high-risk for a build-speed problem that R3 disproved. The cycle is documented in `A3-spec.md` lines 11-13, lines 39-41. Leave it. **No action.**

### 1.3 Module visibility — over-exposure check

`agent-engine/src/lib.rs` declares **every** submodule as `pub mod`:

```
crates/agent-engine/src/lib.rs:4-12
pub mod runtime;     pub mod tools;       pub mod mcp;
pub mod skills;      pub mod events;      pub mod extensions;
pub mod sidecar;     pub mod engine;      pub mod help;
```

This is the *minimum* the TUI layer needs (it reaches into `runtime::subagent`, `tools::shell`, `skills::keybinds`, `extensions::hooks`, `events::queue`, …). Verified by `grep -rEn "use agent_engine::|use synaps_cli::" crates/agent-tui/src`.

By contrast, `agent-tui/src/tui/mod.rs:3-25` correctly declares all child modules as plain `mod` (private), exposing only one entry point at line 45: `pub async fn run(...)`. Clean.

**Verdict:** `agent-engine`'s surface is wide but **not over-exposed** for current consumers. It *is* over-exposed for any *future* external consumer (e.g. someone embedding the engine as a library) — see MEDIUM-4 for a trim-down list once the TUI's needs stabilize.

---

## 2. The facade pattern — soundness

### 2.1 The `extern crate self as synaps_cli` trick

Used in **two** places only:
- `crates/agent-tui/src/lib.rs:7`
- `src/lib.rs:13`

**Not** used in `agent-engine/src/lib.rs`. That is correct: zero engine files reference `synaps_cli::` (`grep -rn "synaps_cli::" crates/agent-engine/src` → 0 hits). Engine code uses `crate::` cleanly.

How resolution works for each consumer scope:

| Scope | `synaps_cli::Runtime` resolves via | End point |
|---|---|---|
| `crates/agent-tui/src/tui/*.rs` | `extern crate self as synaps_cli` → `agent_tui::Runtime` → `pub use agent_engine::Runtime` → `pub use runtime::Runtime` (engine) → `Runtime` struct | one type |
| `src/main.rs`, `src/cmd/*.rs`, `src/watcher/*.rs` | external crate `synaps_cli` (the root `[lib]`) → `pub use runtime::{Runtime,…}` → `pub use agent_engine::{runtime,…}` | same type |

Both paths land on the same `agent_engine::runtime::Runtime`. No type-identity drift. **Sound.**

### 2.2 Re-export chain — same item via parallel paths

`agent-core/src/lib.rs:15-26` re-exports module aliases (`pub use core::config;` etc.) **in addition to** the canonical `pub mod core`. So `agent_core::config` and `agent_core::core::config` both exist and resolve to the same submodule. Same pattern repeats in `agent-engine/src/lib.rs:14-17`, `agent-tui/src/lib.rs:13-17`, and `src/lib.rs:1-24`.

This is the documented "zero-call-site-edits" facade (`A3-spec.md:100-104`). It works, but two consequences worth noting:

- Rustdoc will show the same module under two paths per crate (mildly noisy, not wrong).
- Adding a new top-level module to `agent-core` requires touching **four** `lib.rs` files in lockstep. See NIT-10.

### 2.3 Re-export ambiguity check

I traced every `pub use` for collisions:

- `agent-tui::core` (from `agent_core::core`) does not collide with `agent-tui::runtime`/`tools`/… (from `agent_engine::*`) — different identifiers.
- `agent-engine::Runtime` (struct) vs `agent-engine::runtime` (module) — different namespaces. Rust permits this.
- `agent-tui` re-exports `serde_json::Value` and `tokio_util::sync::CancellationToken` at crate root — also re-exported by `agent-engine` and `src/lib.rs`. Same item via all paths. Fine but noisy (NIT-11).

**No ambiguous re-exports found. No public-API leakage from the facade itself.**

### 2.4 One stale comment

`src/lib.rs:12` — `// Allow intra-crate self-reference via synaps_cli:: (used in src/tui/**)` — `tui/` no longer lives under `src/`. Cosmetic. LOW-8.

---

## 3. Cargo.toml correctness across all four crates

### 3.1 Version & feature consistency for shared deps

| Dep | Root | agent-core | agent-engine | agent-tui | Verdict |
|---|---|---|---|---|---|
| `tokio` | `"1.0"` + `full` | `"1.0"` + `full` | `"1.0"` + `full` | `"1.0"` + `full` | consistent (heavy — see LOW-7) |
| `reqwest` | `"0.12"` + `json,stream,rustls-tls-native-roots`, default-features off | same | same | same | consistent ✓ |
| `serde` | `"1.0"` + `derive` | same | same | same | ✓ |
| `chrono` | `"0.4"` + `serde` | same | same | same | ✓ |
| `crossterm` | `"0.28"` + `event-stream` | — | same | same | ✓ |
| `tachyonfx` | `"0.9"` (no features) | — | — | `"0.9"` + `sendable` | **MISMATCH** — see MEDIUM-3 |
| `signal-hook` | `"0.3"` + `iterator` | — | same | same | ✓ |

### 3.2 Workspace layout

`Cargo.toml:118` — `[workspace] members = ["crates/agent-core", "crates/agent-engine", "crates/agent-tui"]`. The root `synaps` package is *implicitly* a member (Cargo auto-includes a `[package]` in the workspace root unless `exclude`d). That works.

**Critical missing knob:** `resolver = "2"` is **not declared** anywhere. `grep -n resolver Cargo.toml crates/*/Cargo.toml` → 0 hits. See **HIGH-1**.

### 3.3 New deps introduced for #116

- `parking_lot = "0.12"` (agent-tui only) — used in 7 sites, all in `crates/agent-tui/src/tui/render_thread.rs` and one reference comment in `tui/signals.rs:141`. Confined, sensible — the lock-free `Mutex` is the right tool for the render-thread `FrameSlot`. ✓
- `tachyonfx` `sendable` feature — required because `Effect` is sent across the channel into the render thread (`tui/render_thread.rs:56,72,75,253`). ✓ for `agent-tui`. **But** the root `Cargo.toml:46` still declares `tachyonfx = "0.9"` without the feature.

### 3.4 Root-level dep redundancy (audit)

I counted how many of the root's direct deps are actually `use`d from `src/`:

```
unused-by-root direct deps in Cargo.toml:
globset, signal-hook, tower, tokio-tungstenite, tower-http,
urlencoding, base64, sha2, url, dirs, async-trait, tokio-stream,
regex, bytes, arc-swap, arboard, syntect, tachyonfx, unicode-width,
memchr, portable-pty, zeroize, libc
```

That is **23** direct deps that the bin crate does not import. They are pulled transitively via `agent-engine` / `agent-tui` regardless. See **HIGH-2**.

### 3.5 Missing deps

None. Every dep imported by every crate is declared in that crate's manifest.

### 3.6 Duplicated dev-deps

`tempfile` and `serial_test` appear in all four `[dev-dependencies]`. Acceptable (dev-deps are per-crate). Could move to `[workspace.dependencies]`. NIT-10.

---

## 4. Findings — severity ranked

### CRITICAL — none

### HIGH

**H1 · Workspace uses default `resolver = "1"` despite being a multi-crate workspace.**
*Location:* `Cargo.toml:118-119`
*Evidence:* No `resolver` key in any manifest. Workspace root package is edition 2021, but for **workspaces** the root must declare `[workspace] resolver = "2"` explicitly — the edition-default-to-resolver-2 rule only applies to non-workspace packages.
*Impact:* Under resolver 1, features are unified globally across normal + dev + build dependencies, and across all targets. This means: (a) `tachyonfx`'s `sendable` feature unifies up into the bin globally (currently OK), (b) dev-dep features leak into release builds, (c) feature unification can defeat some of the incremental-rebuild win the A3 split was designed to deliver (a fact not reflected in the 2.79s benchmark because the benchmark didn't toggle features).
*Fix:* Add `resolver = "2"` under `[workspace]` in root `Cargo.toml`. Verify nothing breaks (it usually doesn't).

**H2 · 23 redundant direct dependencies in root `Cargo.toml`.**
*Location:* `Cargo.toml:19-78` — `globset, signal-hook, tower, tokio-tungstenite, tower-http, urlencoding, base64, sha2, url, dirs, async-trait, tokio-stream, regex, bytes, arc-swap, arboard, syntect, tachyonfx, unicode-width, memchr, portable-pty, zeroize, libc`.
*Evidence:* `grep -rn` over `src/` shows zero `use` statements for any of them.
*Impact:* Version skew risk — when someone bumps `tokio-tungstenite` in `agent-engine`, root drifts silently. Dilutes the "bin is glue only" claim from `A3-spec.md:84` ("Step 4 — bin = agent-runtime"). Increases the surface that future-Chrollo must reason about when auditing the bin's actual capabilities.
*Fix:* Either delete unused root deps, or — better — promote shared deps into `[workspace.dependencies]` and have each crate `dep = { workspace = true }`. The latter solves H1/H2/M3/NIT-10 together.

### MEDIUM

**M3 · `tachyonfx` feature mismatch between root and `agent-tui`.**
*Location:* `Cargo.toml:46` (`tachyonfx = "0.9"`) vs `crates/agent-tui/Cargo.toml:46` (`tachyonfx = { version = "0.9", features = ["sendable"] }`).
*Impact:* Currently OK because Cargo unifies and the `agent-tui` request wins. But the root's direct dep is **redundant + silently weaker**. If anyone ever consumes the root crate *without* the agent-tui path (e.g. a CI matrix that builds only the lib), `sendable` disappears and `render_thread.rs:56` won't compile. The bug would be invisible until that exact build configuration is exercised.
*Fix:* Delete `tachyonfx` from root `Cargo.toml` (it isn't used by `src/`). If kept, mirror the `sendable` feature.

**M4 · `agent-engine` exposes its entire module tree as `pub mod`.**
*Location:* `crates/agent-engine/src/lib.rs:4-12`.
*Evidence:* Every one of `runtime, tools, mcp, skills, events, extensions, sidecar, engine, help` is `pub mod`. This is required *today* because TUI reaches in (verified — every one is touched by `crates/agent-tui/src/tui/*.rs`).
*Impact:* Any external consumer of `agent-engine` sees the full internal API, including modules likely never meant for external use (`engine::setup`, `engine::commands`, `extensions::config_store`, `extensions::settings_editor`, etc.). The spec's "engine is a runtime, not a library" intent is undermined.
*Fix (deferred):* Once TUI usage settles, narrow visibility — e.g. `pub(crate) mod engine` if nothing outside `agent-engine` needs `engine::setup`. Verify with `cargo check -p agent-tui` after each change.

**M5 · Parallel paths for the same module — `agent_core::config` *and* `agent_core::core::config`.**
*Location:* `crates/agent-core/src/lib.rs:15-26`, mirrored in three other lib.rs files.
*Impact:* Two valid import paths for one item. Works, but: rustdoc shows duplicates; new contributors will inconsistently use one path or the other; a future cleanup will be a sweeping `sed`.
*Fix (optional):* Either drop the `pub mod core` and keep only flat aliases, or drop the flat aliases and force `core::config` everywhere. Spec chose to keep both for migration ergonomics — defensible. **Document the convention** in `agent-core/src/lib.rs` so future contributors don't propagate inconsistency.

### LOW

**L6 · `epoch_millis` was moved to `agent-core` to consolidate timestamp math, but four call sites still inline the same logic.**
*Locations:*
- `crates/agent-core/src/core/auth/storage.rs:99-100`
- `crates/agent-core/src/core/auth/mod.rs:312`
- `crates/agent-engine/src/runtime/telemetry.rs:160, 219, 436, 453, 482`

*Pre-existing (predates A3), but A3 was the right moment.* Mechanical refactor: replace `SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64` with `agent_core::epoch_millis()`.

**L7 · `tokio = { features = ["full"] }` in all four crates.**
*Locations:* root `Cargo.toml:20`, all three crate manifests.
*Impact:* `tokio` "full" pulls 30+ features. `agent-core` almost certainly needs only `rt`, `macros`, `sync`, `time`, `io-util`, `fs`. Build-speed gain available, complementary to A3's stated goal.
*Fix:* Audit per-crate minimum feature set in a follow-up.

**L8 · Stale comment.** `src/lib.rs:12` — `(used in src/tui/**)`. tui lives at `crates/agent-tui/src/tui/`. One-line edit.

**L9 · `dist-workspace.toml` ships only the root bin (`members = ["cargo:."]`).**
This is **intentional** (per `A3-spec.md:131-133`), but the subcrates have full `description` / `license` / `version` metadata as if they were publishable. Add `publish = false` to all three subcrate `Cargo.toml`s to fail-fast on `cargo publish` mistakes.

### NIT

**N10 · The four `lib.rs` facades duplicate the same re-export block.** Adding a new top-level module to `agent-core` requires four edits in lockstep. Worth a `[workspace.dependencies]` migration + a shared macro, or accept the cost and document it.

**N11 · `serde_json::Value` and `tokio_util::sync::CancellationToken` re-exported at the crate root of `agent-engine` and `agent-tui`.** These aren't engine-layer or TUI-layer concerns. Slightly noisy public API. Low value to fix.

**N12 · `agent-engine`'s lib.rs does not need `extern crate self as synaps_cli`, and it correctly omits it.** Worth a one-line comment in `agent-engine/src/lib.rs` explaining *why* engine doesn't need the trick (so a future contributor doesn't "fix" the asymmetry).

---

## 5. Spec compliance

Cross-checked against `docs/specs/A3-spec.md`:

| Spec claim | Status |
|---|---|
| `agent-core` is a clean leaf | ✓ verified |
| `agent-engine` depends on `agent-core` + external only | ✓ verified |
| `agent-tui` depends on `agent-core` + `agent-engine` + render deps | ✓ verified |
| bin imports all three | ✓ verified |
| Internal `runtime↔extensions↔tools` cycle is contained in `agent-engine` | ✓ verified |
| `epoch_millis` moved to `agent-core` | ✓ — see L6 for consolidation opportunity |
| Facade pattern keeps `crate::config` resolving inside each crate | ✓ verified |
| All workspace members build green, 0 warnings | not re-verified (per instructions) — trusted |
| #116 render thread isolated under `agent-tui` | ✓ — `crates/agent-tui/src/tui/render_thread.rs` is the only `parking_lot` consumer |

**No spec deviations.** The merge faithfully implements `A3-spec.md`.

---

## 6. Render-thread (#116) boundary note

The new `render_thread.rs` lives correctly inside `agent-tui` (`crates/agent-tui/src/tui/render_thread.rs`), uses `parking_lot::Mutex<Option<Arc<RenderModel>>>` as a `FrameSlot`, and never touches `agent-engine` or `agent-core` boundaries. The `tachyonfx::Effect` is `Send`-able via the `sendable` feature — flagged for the `agent-tui` Cargo manifest correctly. The boundary cut here is exactly what the A3 split was designed to enable.

---

## 7. Recommended sequencing for follow-up

1. **H1** (add `resolver = "2"`) — five-minute fix, biggest leverage.
2. **M3** (drop or align `tachyonfx` root entry) — five-minute fix.
3. **H2 + N10 together** (migrate to `[workspace.dependencies]`).
4. **L9** (`publish = false` on subcrates) — defensive.
5. **L6** (`epoch_millis` consolidation) — one PR.
6. **M4** (trim `agent-engine` `pub mod` surface) — deferred until TUI usage stabilizes.
7. **L7** (tokio feature minimization) — separate build-speed PR.

— *Chrollo*
