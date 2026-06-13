# Performance Review — `build_render_model` & the Render-Thread Split

**Reviewer:** Yoru (subagent — performance lens)
**Branch / commit:** `dev` @ `17051f2` ("merge: A3 crate split + #116 render thread")
**Scope (read-only):**
- `crates/agent-tui/src/tui/draw.rs` (`build_render_model` / `render_frame`)
- `crates/agent-tui/src/tui/render_model.rs`
- `crates/agent-tui/src/tui/render_thread.rs`
- `crates/agent-tui/src/tui/mod.rs` (throttle, `widget_rx`, tick branch)
- `crates/agent-tui/src/tui/plugins/state.rs` (manual `Clone`)
- `crates/agent-tui/src/tui/settings/mod.rs` (`RuntimeSnapshot`)
- `crates/agent-tui/src/tui/toast.rs` (Toast clones)

No code/tests run. Build was confirmed green by the user.

---

## TL;DR

The Step-1 → Step-2 snapshot split was a structural win (render decoupled from main, fx-clock accurate). But there is **one critical correctness/perf gap** that the docstring on `RenderModel.lines` already hints at: the line cache was *not* promoted to an `Arc<[Line<'static>]>`. Every frame still pays a **full deep clone of the entire wrapped-Line cache** — which can be tens of thousands of owned `String`s during a long session. The render-thread Arc was bolted onto a `Vec`-backed cache, so the "refcount bump, not deep copy" invariant is violated.

That, combined with `widget_rx` events bypassing the 16 ms streaming throttle and triggering a full `build_render_model` per notification, fully explains #119 (~30 % CPU idle with extensions, 0 % without).

The 60 fps mailbox itself coalesces correctly. No over-render at the render-thread side.

---

## Severity-ranked findings

### 🟥 CRITICAL — Per-frame deep clone of the entire line cache
**`crates/agent-tui/src/tui/draw.rs:549–552`**

```rust
let all_lines_vec: &[ratatui::text::Line<'static>] = &app.line_cache.as_ref().unwrap().1;
let total = all_lines_vec.len();
// Wrap in Arc for zero-copy clone into the model.
let lines: std::sync::Arc<[ratatui::text::Line<'static>]> = all_lines_vec.to_vec().into();
```

The comment promises "zero-copy". The code does the opposite. `.to_vec()` allocates a new `Vec<Line<'static>>` of size `total`, **`Clone`-ing every `Line`** (and every `Span` and every `Cow::Owned` `String` inside them). `render_model.rs:39–40` documents the intent — *the `Arc` should be a refcount bump* — but the cache is stored as `Vec<Line<'static>>` (`app.rs:93`) so the conversion is forced to deep-copy.

**Cost reasoning.** `render.rs:14+` builds Lines with `format!()` everywhere → spans hold `Cow::Owned(String)`. A 30-min coding session easily produces 5–20 k cache lines. At 60 fps that is **300 k – 1.2 M `Line` clones/sec** (and ~3× that for span/String allocations). This is the single largest avoidable per-frame cost in the new render path, and it is paid even when **nothing visible changed** because `widget_rx` (see HIGH below) flips `needs_redraw`.

**Fix (one-line change in `app.rs`, two lines in `draw.rs`):**
```rust
// app.rs:93
pub(crate) line_cache: Option<(usize, Arc<[Line<'static>]>)>,

// draw.rs:546–552
let lines = if needs_rebuild {
    let v: Arc<[_]> = app.render_lines(content_width).into();
    app.line_cache = Some((content_width, v.clone()));
    v
} else {
    Arc::clone(&app.line_cache.as_ref().unwrap().1)
};
let total = lines.len();
```

That turns the clone into a single `AtomicUsize::fetch_add`. The downstream `model.lines[start..end].to_vec()` at `draw.rs:933` is fine — it's a tiny visible-only slice.

**Verification angle (not run):** wrap `build_render_model` in a `tracing::debug` span timing it, idle with one extension producing notifications, before/after the Arc fix. Pre-fix: time scales with total message volume. Post-fix: constant.

---

### 🟧 HIGH — `widget_rx` notifications bypass the 16 ms throttle, drive full RenderModel builds per event (#119 root cause)
**`crates/agent-tui/src/tui/mod.rs:196–197, 290–293, 2200–2226`**

The throttle gate is:
```rust
if app.needs_redraw && (!app.streaming || last_draw.elapsed() >= throttle) { … }
```

When **not streaming**, `last_draw.elapsed() >= 16 ms` is bypassed entirely — every `app.needs_redraw = true` causes the *next* loop iteration to fully run `build_render_model`. The widget watcher (line 2200+) spawns one tokio task **per loaded extension** and forwards *every* JSON-RPC notification matched by `is_widget_method` into `widget_tx`. The receiver in the event loop (line 290) calls `app.request_redraw()` **unconditionally** for every event.

Combined with CRITICAL above: an extension that emits notifications at even 20 Hz (a low bar — a clock, a network monitor, a file watcher) produces ~20 full `build_render_model` invocations per second, each deep-cloning the entire wrapped-line cache, settings/plugins state (when modals open), toasts, etc. With several extensions loaded this hits 30 %+ of one core trivially — **exactly the #119 symptom**.

**Confirmed:** widgets without notification traffic → no burn. Extensions that don't push widgets → no burn. The other gated 16 ms tick (line 372) is correctly idle-suppressed — it only calls `request_redraw` inside `if exit_fx_sent || boot_fx_sent || streaming || logo_* || gamba || messages.is_empty()` (line 381). It is **not** the offender.

**Fix options (pick one — they're complementary):**
1. **Throttle idle path too.** Drop the `!app.streaming ||` short-circuit; always apply the 16 ms cap. Latest-wins coalesces a burst into one frame. Cheap.
2. **Coalesce `widget_rx` per loop iteration.** Drain the channel with `while let Ok(ev) = app.widget_rx.try_recv()` before calling `request_redraw` once.
3. **Cheap dirty-hash gate on publish.** Compute a `u64` digest of the cheap RenderModel fields after `build_render_model` and skip `slot.publish` if unchanged. Stops the render thread from re-walking a redraw that produces zero pixel diffs.

(1) + (2) together is the smallest surface fix.

---

### 🟧 HIGH — `RuntimeSnapshot::from_runtime_with_health` does **disk I/O every frame** the settings modal is open
**`crates/agent-tui/src/tui/draw.rs:668–675`** → **`crates/agent-tui/src/tui/settings/mod.rs:100–156`**

```rust
let settings = app.settings.clone().map(|s| {
    let snap = super::settings::RuntimeSnapshot::from_runtime_with_health(...);
    (s, snap)
});
```

Inside `from_runtime_with_health`:
- `synaps_cli::config::load_config()` — reads `~/.synaps-cli/config*` from disk.
- `synaps_cli::skills::loader::load_all(default_roots())` — **walks every plugin dir from disk and parses manifests**.
- `registry.plugin_settings_categories()` and `registry.lifecycle_claims()` — Vec clones (less bad, but still per-frame).

This runs per frame the settings modal is visible. With the streaming throttle disabled on the idle path (HIGH above), one keystroke in settings → one full disk re-scan. Cost reasoning: `load_all` does file-system enumeration + per-plugin manifest parsing — easily 1–10 ms even on warm cache, hundreds of syscalls. At a worst-case 60 fps, that's a steady stream of stat/open/read/parse on the hot path.

**Fix:** snapshot `RuntimeSnapshot` once at modal-open (and on explicit refresh), store it next to `app.settings`, project a cheap reference each frame. The render-model can hold `Arc<RuntimeSnapshot>` and `Arc::clone`. The structure here screams for the Arc pattern that `lines` was *trying* to use.

---

### 🟨 MEDIUM — `app.plugins.clone()` deep-clones `PluginsState` every frame the plugins modal is open
**`crates/agent-tui/src/tui/draw.rs:676`** + **`crates/agent-tui/src/tui/plugins/state.rs:137–152`**

`PluginsModalState::clone` skips the `JoinHandle` (good), but unconditionally clones `file: PluginsState` — which contains `marketplaces: Vec<Marketplace { cached_plugins: Vec<CachedPlugin {...}>, … }>` and `installed: Vec<InstalledPlugin>`. A user with two marketplaces × ~100 cached plugins each = 200 + N small struct + ~5 `String`/Option clones each, per frame.

Not the dominant cost when extensions aren't piping events, but a `widget_rx` burst (HIGH) compounds this: each notification ⇒ full plugins clone if the modal is open.

**Fix:** wrap `PluginsState` in `Arc<PluginsState>`. The render side only reads. Mutations on the App side become `Arc::make_mut`. Same pattern fits `RuntimeSnapshot` (HIGH above) and `lines` (CRITICAL).

---

### 🟨 MEDIUM — Per-frame `Toast` clone deep-copies `rich_lines`
**`crates/agent-tui/src/tui/draw.rs:665`** + **`crates/agent-tui/src/tui/toast.rs:17–30, 159–164`**

```rust
let toasts: Vec<super::toast::Toast> = app.toasts.visible().cloned().collect();
```

`Toast` derives `Clone`. A widget extension's `styled_lines` becomes `rich_lines: Option<Vec<Line<'static>>>` (mod.rs:2049–2073, all `Span::styled(s.text, …)` — owned `Cow::Owned`). Every visible toast is deep-cloned per frame; sticky toasts with multi-line pixel-art content (the explicit motivating use case per the doc comment at toast.rs:23–24) make this a real recurring cost.

Same compounding effect with HIGH #2: notification storms on a sidebar with persistent toasts re-clone them every event.

**Fix:** the render side reads `&Toast`. Either ship `Vec<Arc<Toast>>` (publish-time `Arc::new` once on insert, refcount bump on snapshot), or split toasts into a small render-projection that holds `Arc<[Line<'static>]>` for rich content.

---

### 🟨 MEDIUM — `app.active_tasks.clone()` paid even when empty-of-interest
**`crates/agent-tui/src/tui/draw.rs:709`** + **`crates/agent-engine/src/extensions/active_tasks.rs:56–62`**

`ActiveTasks` = `HashMap<String, TaskState> + Vec<String>`. Clone is cheap when empty (`HashMap::clone` on 0 entries is just bucket-array reset), but `TaskState` holds `Vec<String> recent_logs` (bounded 8) + several `Option<String>`. With an active extension pushing task updates, this gets cloned per frame.

**Fix:** also Arc-wrap. Mutation on App side via `Arc::make_mut`. Or, since render only iterates in order, ship a small `Vec<TaskRowSnap>` projection with just the fields the bar reads.

---

### 🟩 LOW — `ghost_hint` computes `all_commands_with_skills` per frame *only while user is typing a slash command*
**`crates/agent-tui/src/tui/draw.rs:630–662`**

`registry.all_commands()` likely allocates a fresh `Vec<String>` on each call. The whole branch is gated on `app.input.starts_with('/') && len > 1 && no space`, so this only fires when the user is actively editing a slash-completion. Net cost is bounded by typing speed, not 60 fps — fine. Could be memoized on `(input, registry_generation)` if it shows up in profiles, but it's not on the hot idle path. Left as a NIT-adjacent item.

---

### 🟩 LOW — `subagents` / `sidecar_pills` Vec construction
**`crates/agent-tui/src/tui/draw.rs:589–600, 603–627`**

Both rebuild `Vec` per frame. Sizes are tiny (< 16 typically). `sidecar_pills` does a `registry.lifecycle_claims()` clone *every frame regardless of whether sidecars exist* — wait, no, it's correctly gated by `if app.sidecars.is_empty()` (line 604). The `inputs` Vec clones `(plugin_id, display_name)` pairs to feed `order_sidecar_pills`; that's a small allocation but it does mean two passes over the same map. Acceptable.

**Suggestion (not blocking):** `order_sidecar_pills` could take `impl Iterator<Item = (&str, Option<&str>)>` and avoid the intermediate Vec. Saves a few `String` clones × N pills.

---

### 🟩 LOW / NIT — `runtime.model().to_string()` and `runtime.thinking_level().to_string()` per frame
**`crates/agent-tui/src/tui/draw.rs:688–689`**

Two `String` allocs per frame. If these are already `&'static` or interned in the runtime, the allocation is gratuitous. If they're computed, the allocs are unavoidable here. Skim — not in the critical path even at 60 fps.

---

### 🟩 NIT — `from_runtime` (no health) is `#[allow(dead_code)]`
**`crates/agent-tui/src/tui/settings/mod.rs:92–98`**

Unused. Delete or wire it. Not perf-relevant.

---

## Latest-wins mailbox — verified correct
**`crates/agent-tui/src/tui/render_thread.rs:84–108, 246–333`**

`FrameSlot::publish` overwrites the slot under `parking_lot::Mutex` and `unpark`s the thread. The render thread does `while let Some(model) = inner.lock().take()` — this drains exactly one frame per pass, then re-parks if the slot is empty. Spurious wakeups loop benignly into a no-op `take()`. A burst of `publish` calls between two passes is correctly coalesced to the last one. **No over-render at the render thread.** The over-build is exclusively on the *main* side (see HIGH #1).

One subtle observation: between `lock().take()` returning `Some` and `render_frame` finishing, the main task can `publish` a new frame that immediately gets picked up at the top of `while let`. That's *intended* (it keeps the screen current), but it does mean a fast-publishing main side can saturate the render thread back-to-back without ever re-parking. Mitigated entirely by fixing HIGH #1 (no idle floods).

---

## OLD direct-draw vs NEW snapshot — net assessment

| Aspect | Old (`draw(&mut App, …)` inline) | New (snapshot → render thread) |
|---|---|---|
| Line cache access | Borrowed `&[Line]` directly. Zero per-frame alloc. | **Full `Vec` deep clone every frame** (CRITICAL). |
| Settings/plugins/toasts read | Borrowed from `App`. Zero clones. | Cloned per frame. |
| Render thread independence | None — main task blocked on terminal writes. | ✅ Big win for tail latency / fx-timing accuracy. |
| Terminal-write/fx interleave | Coupled. | ✅ Decoupled, accurate. |
| Bounded teardown | Custom watchdog (#116-old). | ✅ Clean self-bounding via ack channel. |

**Verdict.** The split is the right architecture. But the migration **regressed per-frame allocation** because the Arc'd-handle pattern (the lever that was supposed to make snapshots free) was applied to projections (toasts, subagent snaps, etc.) and **not** to the structurally heavy pieces (`lines`, `RuntimeSnapshot`, `PluginsState`). The CRITICAL + first HIGH are both versions of the same root cause: `Arc<T>` is doing the work of a refcount bump for *projection* types, but the *heavy* App-owned structures still use `Vec`/`HashMap` `Clone`.

If you fix CRITICAL + both HIGHs, the snapshot approach becomes a clean **net win**: terminal-write decoupled, ~zero per-frame deep-cloning, fx timing accurate. Until then, the new path has a higher per-frame floor than the old `&App` direct draw under the #119 conditions.

---

## Recommended order of attack

1. **CRITICAL — line cache → `Arc<[Line<'static>]>`.** ~3 line change in `app.rs` + 3 in `draw.rs`. Biggest single win.
2. **HIGH — apply the 16 ms throttle on the idle path too** (drop `!app.streaming ||`), and drain `widget_rx` in a loop before calling `request_redraw` once. Kills #119 directly.
3. **HIGH — `RuntimeSnapshot` snapshotted at modal-open** (stored on App, `Arc`'d into the model). No disk I/O on the render hot path.
4. **MEDIUM — Arc-wrap `PluginsState` and `ActiveTasks`.** Same pattern, same pay-once strategy.
5. **MEDIUM — `Vec<Arc<Toast>>` (or split-projection)** to refcount-bump rich lines.
6. (Optional, after profiling shows residual cost) cheap dirty-hash gate before `slot.publish`.

---

## What to watch next

- After fixing the line-cache Arc: the next hot point will likely be `render_lines` itself when the cache is invalidated (every `push_msg`, every `advance_animations` returning true). Long sessions = `O(messages)` rebuild on every message append. Worth investigating an *incremental* cache (append-only segments keyed by message index) once the snapshot path is clean.
- `tick` branch (`mod.rs:372`) has a long disjunction — easy to accidentally extend it with a condition that's not actually animation-related. Each addition is effectively a "force 60 Hz wake" toggle. Treat as a guarded surface.
- `advance_animations` invalidates the *entire* line cache (`mod.rs:427–429`). Once the cache is `Arc`-promoted, that invalidation drops the Arc and forces a re-render of every line. Per-message animation regions should probably be tracked separately so cache invalidation can stay localized.
- Profile with `cargo flamegraph` under the #119 reproduction (idle, one extension emitting notifications). Pre-fix flamegraph should be dominated by `Line::clone` / `Span::clone` / `String::clone` under `build_render_model`. That'll confirm CRITICAL is the bottleneck quantitatively.
