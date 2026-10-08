# Spec: Configurable Prompt-Cache TTL (`cache_ttl`)

**Status:** v3 — post-implementation review fix: §3.4.1 amended with the saw-1h-honored condition (the v2 detector condition false-positived every healthy Hybrid session). Previously: v2 — amended per adversarial review (APPROVED WITH AMENDMENTS A1–A8); verified against source @ dev HEAD `ffa83a8`
**Author:** Zero (architect pass; verification sweep completed against actual code, not the brief; reviewer amendments applied without relitigation)
**Scope:** `synaps` (Cargo package `synaps`, lib `synaps_cli`), Anthropic runtime path only

---

## ⚠️ 0. Discrepancy Notice — Brief vs. Code (READ FIRST)

The brief's **identity claim is wrong**. The brief asserts the runtime's default
system-prompt identity is the *Claude Code* identity ("You are Claude Code,
Anthropic's official CLI for Claude"). The code says otherwise:

- `src/core/config.rs:10` —
  `const DEFAULT_IDENTITY: &str = "You are an AI assistant running in SynapsCLI, an open-source agent runtime.";`
- The **stale doc comment** directly above `get_identity()` (config.rs:~12–14) still reads
  *"Falls back to the default Claude Code identity if not set in config."* — this comment is
  the likely source of the brief's claim. The comment lies; the constant does not.
- `get_identity()` is consumed at both OAuth system-block build sites
  (`src/runtime/api.rs` ~line 446, `src/runtime/api_sync.rs:121`) as the **first** system
  block — i.e., the head of the cached system prefix.

**Why this matters for this spec:** the identity block is the first block of the
cacheable system prefix. Any identity change (including "fixing" it to match the brief)
invalidates the cache prefix for every active session. **Do not change the identity as
part of this work.** Fix only the stale doc comment (zero wire impact). The brief's
claim is recorded here so the builder doesn't "helpfully" align code to brief.

---

## 1. Purpose

Anthropic prompt caching supports two ephemeral TTLs: **5 minutes** (default,
1.25× input price on write) and **1 hour** (2.0× input price on write, opt-in
via `cache_control: {"type": "ephemeral", "ttl": "1h"}` plus the
`extended-cache-ttl-2025-04-11` beta header).

Today the runtime hardcodes bare `{"type": "ephemeral"}` (implicit 5m) at every
marker site. Long-lived agent sessions, watchers, and server-mode deployments
with sparse turn cadence (> 5 min between calls) get **zero cache hits** and pay
full input price every turn. This spec introduces a configurable TTL with
correct pricing, telemetry, and protocol plumbing.

The read path is already TTL-aware (telemetry parses the 5m/1h write breakdown);
only the **write path, pricing, and config surface** are missing. We complete
the circuit.

---

## 2. Verified Current State (ground truth, from code)

### 2.1 Cache marker sites (4 logical sites × 2 transports = 8 code sites)

| # | Marker | Streaming (`src/runtime/api.rs`) | Sync (`src/runtime/api_sync.rs`) |
|---|--------|----------------------------------|----------------------------------|
| 1 | Last message block — `HelperMethods::annotate_cache_breakpoint` (`src/runtime/helpers.rs:133–144`) | called at `api.rs:401` | called at `api_sync.rs:82` |
| 2 | Last tool in `tools` array | `api.rs:439` | `api_sync.rs:114` |
| 3 | Last system block (OAuth path) | `api.rs` (~446) | `api_sync.rs:128` |
| 4 | Sole system block (API-key path) | `api.rs` (~452) | `api_sync.rs:133` |

All eight emit exactly `json!({"type": "ephemeral"})`. The message-marker strategy
is **single-last** (S204 benchmark: 96–97% hit rate, matches sliding-4, eliminates
the prefix-invalidation bug class — see doc comment at `helpers.rs:128–132`).
`annotate_cache_breakpoint` coerces string content to a block array before marking,
and marks only the final block of multi-block content (unit tests at
`helpers.rs:386–459` pin this behavior).

### 2.2 Pricing — `src/pricing.rs`

Single source of truth, per its own module doc. `calculate_cost(model, input,
output, cache_read, cache_creation)` bills cache reads at **0.10×** input and
cache writes at **1.25×** input — the 1.25× is documented as "(5-minute TTL write)"
at `pricing.rs:19`. There is **no 1h write rate**. Model table: Fable $10/$50,
Opus $5/$25, Sonnet $3/$15, Haiku $1/$5; substring match; unknown → Sonnet.

Call sites: `src/engine/session.rs:109` (`SessionState::add_usage`) and
`src/tui/app.rs:422`. Both pass the four token counts straight through.

### 2.3 Usage plumbing (read path — already TTL-aware)

- Wire: `src/runtime/sse_types.rs:114–116` parses
  `usage.cache_creation.{ephemeral_5m_input_tokens, ephemeral_1h_input_tokens}`
  (tests at sse_types.rs:293–324 cover `message_start` and `message_delta` shapes).
- Telemetry: `src/runtime/telemetry.rs` `UsageRecord` carries `cache_write_5m` /
  `cache_write_1h` (`Option<u64>`, skip-if-none) + `hit_pct`; populated at
  `api.rs:272–277` from `message_delta` only (telemetry-gated).
- **Gap:** `SessionEvent::Usage` (`src/runtime/types.rs:53–54`) and
  `EngineStreamEvent::Usage` (`src/engine/stream.rs:166–176`) carry only the
  aggregate `cache_creation_input_tokens` — the TTL split is dropped before it
  reaches cost accounting. RPC mirror: `docs/rpc-protocol.md` §4.6 `usage`
  object inside `agent_end` likewise has only the aggregate.

### 2.4 Config & Runtime

- `SynapsConfig` (`src/core/config.rs:155+`): flat key list, known-keys array at
  config.rs:219 (includes `telemetry`, `cache_diagnostics`, `identity`, …).
  Unknown keys produce non-fatal `warnings` surfaced at boot.
- `Runtime` (`src/runtime/mod.rs:141+`): owns per-request knobs (`api_retries`,
  `telemetry_level`, `cache_diagnostics: bool` with getter/setter at
  mod.rs:385–390, threaded from config at mod.rs:330). Per-request options
  travel via `ApiOptions` (`src/runtime/api.rs:331`), constructed at
  mod.rs:441 (`run_single`) and mod.rs:730 (`run_stream` →
  `StreamSession.options`); the manual `Clone` impl for `Runtime` at
  mod.rs:757–786 is a struct literal, so new fields are compiler-enforced.
  There is already precedent for a beta header builder: `build_beta_header`
  lives at `src/runtime/request.rs:33–51` (api.rs:494 is merely the call
  site); note that `api_sync.rs` `call_api` carries a **duplicated inline
  beta builder at :154–164** — a latent divergence this spec eliminates (§3.4).
- Settings modal (`src/tui/settings/mod.rs`): **no cache-related entries today**
  (verified; only an unrelated model-ping cache comment at line 59).

---

## 3. Design

### 3.1 Config surface

New key in `SynapsConfig`:

```rust
/// Prompt-cache TTL strategy: "5m" (default) | "1h" | "hybrid".
pub cache_ttl: CacheTtl,   // enum, Default = FiveMinutes
```

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheTtl {
    #[default]
    FiveMinutes,   // wire: omit ttl field entirely (today's behavior, byte-identical)
    OneHour,       // wire: "ttl": "1h" on all markers + beta header
    Hybrid,        // wire: "ttl": "1h" on stable-prefix markers (tools+system),
                   //       bare ephemeral (5m) on the message-tail marker + beta header
}
```

Parsing (config.rs key-match block, alongside `cache_diagnostics` at :461):

| Input (case-insensitive) | Result |
|---|---|
| `5m`, `5min`, `default`, unset | `FiveMinutes` |
| `1h`, `60m`, `1hr` | `OneHour` |
| `hybrid` | `Hybrid` |
| anything else | `FiveMinutes` + push to `config.warnings` (never block boot — house rule) |

Add `"cache_ttl"` to the known-keys array at config.rs:219.

**Rationale:** an enum, not a String, because there is a closed set of valid
strategies and Anthropic rejects unknown wire `ttl` values with a 400. Invalid
config must degrade to the safe default *with a warning*, consistent with the
file's existing contract ("non-fatal problems… never block boot").

### 3.2 Runtime threading

- Add `cache_ttl: CacheTtl` to `Runtime` (mod.rs:141 struct), default
  `FiveMinutes` in the constructor (~:229), assign from config in
  `apply_config` (~:330), expose `cache_ttl()` / `set_cache_ttl()` getters
  mirroring the `cache_diagnostics` pair (:385–390).
- Thread into requests by adding `cache_ttl` to **`ApiOptions`**
  (`src/runtime/api.rs:331`). It is constructed at two sites, both of which
  must populate the field: mod.rs:441 (`run_single`) and mod.rs:730
  (`run_stream` → `StreamSession.options`). The manual `Clone` impl for
  `Runtime` at mod.rs:757–786 is a struct literal and picks up the new field
  under compiler enforcement — no action beyond adding the field, but the
  builder should expect the compile error there as the checklist.
  (*Correction from v1: the previous "request-options bundle at mod.rs:~777"
  reference was wrong — that line is the manual `Clone` impl, not an options
  bundle.*)

### 3.3 Wire emission — one role-aware helper, eight sites collapse to one truth

Add to `HelperMethods` (helpers.rs):

```rust
/// Where a cache_control marker sits in the request body.
/// Emission order is tools → system → messages, so StablePrefix markers
/// always precede the MessageTail marker — this ordering is what makes
/// the Hybrid combination legal under Anthropic's rule that longer-TTL
/// breakpoints must precede shorter-TTL ones.
pub(super) enum MarkerSite {
    StablePrefix,  // tool marker, OAuth system marker, API-key system marker
    MessageTail,   // annotate_cache_breakpoint's last-message marker
}

/// The single source of the cache_control JSON value. Bare ephemeral (5m)
/// omits `ttl` entirely — byte-identical to today's payloads, so the
/// default path cannot invalidate existing cached prefixes.
///
/// Cost trade-off (honest numbers, not marketing):
/// - 5m:     1.25× write everywhere. Wins for rapid-fire sessions.
/// - 1h:     2.0× write everywhere. Wins for SPARSE long sessions — note
///           that under Hybrid, every >5m gap forces a 5m re-write of the
///           message tail, and that tail write covers the WHOLE conversation
///           since the system breakpoint, not just the increment. Uniform 1h
///           avoids that repeated full-tail re-write.
/// - Hybrid: 2.0× write on the stable prefix (tools+system, written rarely),
///           1.25× on the message tail (written every turn). Wins for
///           bursty / medium-gap cadence: cheap per-turn writes while the
///           expensive prefix survives gaps up to 1h.
pub(super) fn cache_control_value(ttl: CacheTtl, site: MarkerSite) -> Value {
    match (ttl, site) {
        (CacheTtl::FiveMinutes, _)                 => json!({"type": "ephemeral"}),
        (CacheTtl::OneHour, _)                     => json!({"type": "ephemeral", "ttl": "1h"}),
        (CacheTtl::Hybrid, MarkerSite::StablePrefix) => json!({"type": "ephemeral", "ttl": "1h"}),
        (CacheTtl::Hybrid, MarkerSite::MessageTail)  => json!({"type": "ephemeral"}),
    }
}
```

- `annotate_cache_breakpoint(messages: &mut [Value])` →
  `annotate_cache_breakpoint(messages: &mut [Value], ttl: CacheTtl)`; it calls
  the helper with `MarkerSite::MessageTail`; both callers (api.rs:401,
  api_sync.rs:82) pass the runtime value.
- Replace the three inline `json!({"type": "ephemeral"})` literals in each
  transport (tool marker, OAuth system marker, API-key system marker) with
  `cache_control_value(ttl, MarkerSite::StablePrefix)`.

**Invariant:** TTL mixing within a request is impossible by construction
*except* for the one sanctioned combination: Hybrid's 1h-prefix / 5m-tail. This
satisfies Anthropic's ordering rule (longer TTL before shorter) given our fixed
tools→system→messages emission order, and is the only mix with sane economics.
(*Correction from v1: the prior exclusion rationale refuted an inverted
configuration nobody proposed — a 1h message marker behind a 5m system marker.
The actual hybrid puts 1h in front, where it belongs.*)

### 3.4 Beta header — auth-aware, one builder, two transports

Append `extended-cache-ttl-2025-04-11` to `anthropic-beta` **only when both
hold**: `auth_type != "oauth"` **and** `cache_ttl ∈ {OneHour, Hybrid}`.

- **OAuth path sends no new beta token.** Live probe confirmed 1h TTL works
  bare on OAuth (`ephemeral_1h_input_tokens=1457` observed without the
  header). The OAuth beta set is part of the pool-routing fingerprint and is
  empirically hair-trigger — we do not perturb it for a feature that works
  without it.
- The logic lives in `build_beta_header` at `src/runtime/request.rs:33–51`
  (api.rs:494 is the call site; comma-joined with the existing 1M-context
  token when both apply).
- **REQUIRED refactor:** `api_sync.rs` `call_api` currently duplicates the
  beta builder inline at :154–164. This spec mandates replacing that inline
  block with a call to `build_beta_header`. One builder, two transports —
  otherwise the auth-aware gating exists in one transport and silently not
  the other.

When `FiveMinutes`, emit nothing new — requests stay byte-identical to today.

**Edge case:** if the account/model rejects the beta (400 with beta-related
error), the existing retry loop treats 400 as non-transient and surfaces the
error. Acceptable for v1; the error message names the config key (`cache_ttl`)
so the user can self-serve the fix. Do **not** silently downgrade — silent
pricing changes are how trust dies.

### 3.4.1 Silent-downgrade detector

The failure mode that *doesn't* 400: the API accepts the request but quietly
honors only 5m. Detection: when `cache_ttl ∈ {OneHour, Hybrid}` and a
response's `cache_creation` split shows the **1h bucket = 0 while the 5m
bucket > 0**, *and the session has never seen a nonzero 1h bucket*, emit a
one-time-per-session `SessionEvent::Notice` —
*"1h cache TTL not honored — check account/beta support."* The split is
already parsed (`sse_types.rs:114–116`); this is a comparison and two
latches, nothing more.

**Where the split is actually read (v4 amendment, from `534ea80`):** live
streaming traffic carries the `cache_creation` sub-object **only on
`message_start`** — `message_delta` usage is aggregate-only. The streaming
parser therefore captures the split in the `message_start` arm
(`ParseState.msg_start_cache_5m/_1h`) and the `message_delta` arm — where the
detector, the `saw_1h_honored` latch, and the telemetry/`SessionEvent::Usage`
emission all live — uses the delta's own sub-object when present
(future-proof) and otherwise falls back to the `message_start` capture. Any
implementation that reads the split exclusively in the delta arm has a dead
live path: the split is permanently `None`, the detector can never latch or
fire, and 1h writes get billed at the 5m rate. Synthetic fixtures must mirror
the live shape (start carries the split, delta omits it) or they will mask
this. The sync transport has no start/delta dichotomy — the non-streaming
response body carries `usage.cache_creation` directly.

**Where the split is actually read (v4 amendment, from `534ea80`):** live
streaming traffic carries the `cache_creation` sub-object **only on
`message_start`** — `message_delta` usage is aggregate-only. The streaming
parser therefore captures the split in the `message_start` arm
(`ParseState.msg_start_cache_5m/_1h`) and the `message_delta` arm — where the
detector, the `saw_1h_honored` latch, and the telemetry/`SessionEvent::Usage`
emission all live — uses the delta's own sub-object when present
(future-proof) and otherwise falls back to the `message_start` capture. Any
implementation that reads the split exclusively in the delta arm has a dead
live path: the split is permanently `None`, the detector can never latch or
fire, and 1h writes get billed at the 5m rate. Synthetic fixtures must mirror
the live shape (start carries the split, delta omits it) or they will mask
this. The sync transport has no start/delta dichotomy — the non-streaming
response body carries `usage.cache_creation` directly.

**Saw-1h-honored condition (v3 amendment):** the naive `1h == 0 && 5m > 0`
test is the steady-state signature of a *healthy* Hybrid session — on turn 2+
the 1h prefix is cached (zero 1h writes) while the 5m tail rewrites every
turn — so it false-positives every Hybrid session. The detector therefore
requires a second session-scoped latch, `saw_1h_honored`, set whenever a
response's 1h bucket is **> 0**. A healthy session sets it on turn 1 (prefix
write) and the detector stays silent thereafter; a genuinely downgraded
account never sets it, so the notice still fires on turn 1. Like the notice
latch, it is shared across all requests in a session and reset fresh for
cloned (subagent) runtimes.

**Latching rule:** never auto-flip the configured mode mid-session. The notice
fires once and the runtime keeps requesting what the user configured —
auto-downgrade would change pricing behavior behind the user's back and mask
the account-level problem the notice exists to surface.

### 3.5 Pricing — make the TTL split first-class

`pricing.rs` gains the 1h write rate and a split-aware entry point:

```rust
// Cache pricing (relative to input price):
// - reads: 0.10× | 5m write: 1.25× | 1h write: 2.0×
pub fn calculate_cost_split(
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_read: u64,
    cache_write_5m: u64,
    cache_write_1h: u64,
) -> f64
```

Keep the existing `calculate_cost` signature as a wrapper:
`calculate_cost(m, i, o, r, w) == calculate_cost_split(m, i, o, r, w, 0)` —
i.e., aggregate writes are billed at 5m rate when no split is available. This
preserves every existing call site and test, and under-bills only when the user
has *opted into* 1h and the split somehow didn't arrive (fail-cheap, not
fail-expensive — cost display is informational, not invoiced).

### 3.6 Event & RPC plumbing (close the gap from §2.3)

1. `SessionEvent::Usage` (types.rs:53) gains
   `cache_creation_5m: Option<u64>`, `cache_creation_1h: Option<u64>` —
   populated at both emission sites in api.rs. *(v4 correction: the v3 text
   said `message_delta` "already has `usage.cache_creation` in scope" — true
   at the type level, but live deltas never carry the sub-object; the delta
   emission must use the `message_start` capture per §3.4.1.)* `None` when
   the API omits the sub-object.
2. `EngineStreamEvent::Usage` (engine/stream.rs:166) mirrors the two fields.
3. `SessionState::add_usage` (engine/session.rs:109) — **change the existing
   method's SIGNATURE** to accept the split (two `Option<u64>` parameters or a
   struct); do **not** add a parallel method. Rationale (the review's severest
   defect): the server path destructures usage events with `..`, so a parallel
   method leaves every existing caller silently absorbing the new fields —
   server mode would bill 1h writes at 1.25× forever, with no compile error
   and no symptom. A signature change forces every call site to confront the
   split. The TUI accumulator (tui/app.rs:422) likewise calls
   `calculate_cost_split` when the split is `Some`, falling back to the
   wrapper otherwise.
4. Server mode: `src/cmd/server.rs` — the usage-thread split at :737–752 flows
   into the (re-signed) `add_usage`; extend `ServerMessage::Usage` at :807–814
   with the split fields.
5. RPC: `src/cmd/rpc.rs:172` accumulates the split;
   `src/core/rpc_protocol.rs` `TurnUsage` gains two `Option<u64>`
   skip-if-none fields. `src/cmd/chat.rs:239` does an exhaustive destructure
   and will be compiler-caught — update it.
6. `docs/rpc-protocol.md` §4.6: add the two optional fields to the `usage`
   object, documented as *optional, omitted when unknown* — additive, so
   existing protocol consumers are unaffected.

### 3.7 Settings modal & introspection (minimal v1)

The settings modal currently exposes no cache controls; adding a toggle row is
optional polish, **not** in the critical path. Any future `/cache-ttl` slash
command or settings row **must** route through the live engine dispatch path —
`engine/commands.rs` intercepts before `tui/commands.rs` (the `ffa83a8`
lesson). Required for v1: the config key,
and the `usage.log` line (`HelperMethods::log_usage`, helpers.rs:180–227)
gains nothing — it logs aggregates and stays stable for any external parsers.
Telemetry (`UsageRecord`) already carries the split; no change.

---

## 4. Edge Cases

| Case | Behavior |
|---|---|
| `cache_ttl` unset / invalid | 5m default + boot warning; payloads byte-identical to current release |
| Mixed TTL within one request | Impossible by construction except the sanctioned Hybrid combination (1h prefix / 5m tail) — §3.3 |
| 1h requested but only 5m honored (no 400) | One-time-per-session Notice; mode never auto-flipped (§3.4.1) |
| TTL changed mid-session (`set_cache_ttl`) | Next request re-marks with new TTL; old prefix expires naturally; no invalidation logic needed because single-last strategy never prunes old markers |
| Provider models (OpenAI-compat path, `src/runtime/openai/*`) | Out of scope — `cache_ttl` is Anthropic-only; the openai translate layer never sees `cache_control` (it strips/ignores it today; verify in tests, do not change) |
| `cache_creation` sub-object absent in SSE | Split fields `None`; cost falls back to 5m-rate aggregate (§3.5) |
| 1h + cache-diagnosis beta both on | Two tokens in one `anthropic-beta` header, comma-joined — `build_beta_header` already owns this concern |

## 5. Test Plan

- `helpers.rs`: extend the existing `annotate_cache_breakpoint` test block
  (:386–459) — 5m emits no `ttl` key (assert absence, not just type), 1h emits
  `"ttl":"1h"`, multi-block/coercion behavior unchanged under all modes.
- `cache_control_value`: **exact-string unit assertions** (no snapshot harness
  exists in this repo; a byte-identical snapshot test is unimplementable as
  specified in v1 and is hereby replaced). E.g.
  `serde_json::to_string(&cache_control_value(FiveMinutes, _)) == r#"{"type":"ephemeral"}"#`,
  plus the full per-mode × per-site matrix. Extracting body construction into
  a testable unit is **not** required for v1 — the exact-string assertions on
  the single helper, plus per-transport marker-site tests, pin the wire.
- Hybrid: assert a hybrid-mode request body carries `"ttl":"1h"` on the tool
  and system markers and **no** `ttl` key on the message marker; assert the
  ordering invariant (every 1h marker precedes the 5m tail marker in body
  emission order).
- Per-transport marker-site tests: both `api.rs` and `api_sync.rs` paths
  produce the correct marker value at each of their four sites for each mode.
- `pricing.rs`: 1M-token 1h write on Sonnet = $6.00 (2.0 × $3); wrapper
  equivalence property (`calculate_cost == calculate_cost_split(.., w, 0)`).
- `config.rs`: parse table from §3.1 incl. `hybrid` and warning on garbage value.
- `sse_types.rs`: no change — existing split-parsing tests already pin the wire.
- Downgrade detector: synthetic response with 1h bucket = 0, 5m bucket > 0
  under `OneHour`/`Hybrid` emits exactly one Notice per session; second
  occurrence emits nothing; mode unchanged.

## 6. File-by-File Change List

| File | Change |
|---|---|
| `src/core/config.rs` | `CacheTtl` enum (3 variants), `cache_ttl` field + parse + known-key; fix stale "Claude Code identity" doc comment (§0) |
| `src/runtime/mod.rs` | `Runtime.cache_ttl` field, ctor default, config wiring, getter/setter; `ApiOptions` construction at :441 and :730; `Clone` impl :757–786 (compiler-enforced) |
| `src/runtime/request.rs` | `build_beta_header` (:33–51): auth-aware `extended-cache-ttl-2025-04-11` token (§3.4) |
| `src/runtime/helpers.rs` | `MarkerSite`, `cache_control_value()`, TTL param on `annotate_cache_breakpoint`, tests |
| `src/runtime/api.rs` | `ApiOptions.cache_ttl` (:331); 4 marker sites use helper; Usage events carry split; downgrade-detector latch |
| `src/runtime/api_sync.rs` | Same marker work, sync transport; **replace inline beta builder (:154–164) with `build_beta_header` call** |
| `src/runtime/types.rs` | `SessionEvent::Usage` split fields; `SessionEvent::Notice` for downgrade detector |
| `src/engine/stream.rs` | `EngineStreamEvent::Usage` split fields |
| `src/engine/session.rs` | **`add_usage` signature change** (:109) → split-aware cost; no parallel method (§3.6) |
| `src/tui/app.rs` | Cost accumulator → split-aware cost (:422) |
| `src/cmd/server.rs` | Thread split into `add_usage` (:737–752); extend `ServerMessage::Usage` (:807–814) |
| `src/cmd/rpc.rs` | Accumulate split (:172) |
| `src/core/rpc_protocol.rs` | `TurnUsage` gains two `Option<u64>` skip-if-none fields |
| `src/cmd/chat.rs` | Exhaustive destructure at :239 — compiler-caught, update |
| `src/pricing.rs` | `calculate_cost_split`, 2.0× rate, doc table, tests |
| `docs/rpc-protocol.md` | §4.6 optional split fields |

---

## 7. Cache keep-warm (`cache_keepwarm_hours`, task #444)

**Opt-in, off by default.** With `cache_ttl = 1h`, an interactive session that
goes idle can keep its cached prefix alive with cheap cache-read pings, so
coming back after lunch (or the next morning) reads the 300k–1M token prefix
at 0.1× input price instead of rewriting it at 2×.

```
cache_ttl = 1h
cache_keepwarm_hours = 4   # 0 = off (default); max 24
```

### 7.1 What it costs

A ping re-sends the exact prefix and reads it from cache. Per ping, on a
prefix of `P` tokens at input price `p`:

| item | cost | Opus ($5 in / $25 out), P = 500k |
|---|---|---|
| cache read of the prefix | `0.1 × P × p` | $0.25 |
| trailing ping message (~25 tokens, uncached) | `25 × p` | ~$0.0001 |
| output (hard cap 16 tokens, thinking included) | `≤ 16 × 5p` | ≤ $0.0004 |
| first ping only: writes the final assistant reply (`W` tokens) at 1h | `2 × W × p` | $0.02 for W = 2k |
| **rewrite it replaces** (user returns after expiry) | `2 × P × p` | **$5.00** |

One ping costs ~5% of one rewrite (models with 0.05× / 0.025× cache reads —
Opus/Sonnet 5.5, Fable/Mythos 5.1 — make it 2.5% / 1.25%). Pings run every
55 minutes, so a 4 h window costs 4 pings (≈ 0.4 P input-equivalents) and pays
off if at least one in five idle periods ends with the user coming back inside
the window. **Abandoned sessions pay for every ping in the window**, which is
why it is opt-in and capped.

### 7.2 When it runs

- Owner: the session actor (`crates/agent-engine/src/session/cache_keepwarm.rs`).
  It arms when the actor announces `Idle` after a turn that reached the
  provider and whose history ends in a finished assistant reply, with an
  interactive client (TUI or `synaps attach`) attached.
- First ping at *last request start + 55 min* (the TTL counts from the start
  of the request that last read/wrote the entry; 5 min margin). Each ping
  re-schedules *ping start + 55 min* while that is inside
  *idle start + `cache_keepwarm_hours`*.
- Stops on: any input command from a client, a turn or compaction starting,
  the last interactive client detaching, park, session end,
  `max_session_cost` reached (session cost + ping cost), or any ping error.
  It re-arms only at the next idle.
- Never runs for subagents, headless/one-shot sessions, RPC/server clients,
  armed session drivers, non-Anthropic models, `cache_ttl = 5m` or `hybrid`
  (the history marker is 5m there), or budgeted thinking (below). The reason
  is logged once (`cache keep-warm off: …`).

### 7.3 Why the ping hits the cache

The ping is built by `Runtime::cache_keepwarm_ping`
(`crates/agent-engine/src/runtime/keepwarm.rs`) through the same request
builder as a turn (`call_api_stream_inner` → `build_anthropic_request`): same
tools (including the progressive-disclosure projection), system prompt,
thinking config and effort, model, TTL markers and durable history. The only
differences are after the cache key's reach:

- the conversational `cache_control` marker sits on the **last history
  message** instead of the new user message, and the trailing ping message
  (`[automated prompt-cache keep-warm ping — not from the user] Reply with
  only: ok`) rides after it unmarked. The next real turn's lookback finds
  this entry, so it reads through the final assistant reply;
- `max_tokens = 16` (`max_tokens` is not part of the cache key);
- no memory recall, `before_message`/Inject hooks, steering or pressure
  notice (all request-tail, per-turn content), no retries.

The ping never touches history, the session file, the transcript or the
session's cost counters; its spend is logged per ping and counted against
`max_session_cost`. Telemetry records for pings carry `"keepwarm": true`.

### 7.4 Thinking

Thinking config (mode, `budget_tokens`) and `output_config.effort` are part of
the cache key, so the ping must carry them unchanged.

- **Adaptive thinking** (Opus 4.7+, Sonnet 4.7+, 5.x) and **thinking off**:
  `max_tokens` is a hard cap on thinking + text, so output is ≤ 16 tokens
  (≤ $0.0004 on Opus). Supported.
- **Budgeted thinking** (Opus 4.6, Sonnet 4.6/4.5, Haiku 4.5 with thinking
  on): the API requires `max_tokens > budget_tokens`, and the budget can't
  change without invalidating the cache, so one ping could bill up to the full
  budget — 16,384 tokens × $25/MTok = $0.41 on Opus 4.6 at the default level,
  more than the $0.25 cache read it buys. Keep-warm refuses these
  configurations rather than spend silently.

---

*Every component here exists because the code demanded it; nothing exists
because the brief asserted it. The brief was wrong once — see §0. Build from
this document, not from the brief.*
