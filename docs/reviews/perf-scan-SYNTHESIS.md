# Perf/Scaling Audit Synthesis (S208)

4 scanners, full repo. Individual reports: docs/reviews/perf-scan-{storage,runtime,boot,tui}.md.
Root theme: **doing O(total data) work for a small need** — gets slower as sessions/conversations grow.

## Already fixed (committed)
- **latest_session()** — mtime lookup, not parse-all-221-files. ~11s boot → ~1s. (commit a0aadb7)
- **list_sessions()/list_recent_sessions()** — header-read (stop at "api_messages"), recent-only for
  /sessions. 76MB parse → 3ms. (commit 7bf046c)

## Being fixed now (S208 autonomous, 4 parallel agents)
1. **save_chain 76MB scan for a cosmetic warning** (chain.rs:45) — dropped. [spike]
2. **find_session_by_name single-scan + /resume double-scan** — confirm/dedupe. [spike + tui agent]
3. **skills::register sync fs-walk on tokio thread** (~1.4s boot) — spawn_blocking. [case]
4. **plugins.json re-read 7× in the extension load loop** — hoist out of loop. [case]
5. **serial extension load** (7 × await) — JoinSet parallel (if safe). [case]
6. **sanitize_thinking_blocks O(N²) per turn** (helpers.rs:95) — tail-only. [joestar]
7. **retry re-serializes 827-msg body 8×** (api.rs:644) — serialize once → Bytes. [joestar]
8. **/extensions audit reads whole unbounded log** (mod.rs:1136) — bounded tail. [tui spike]
9. **watcher logs --follow re-reads whole file every 500ms** — incremental seek. [tui spike]

## DEFERRED — bigger/riskier, tracked as tasks (NOT done autonomously)
- **A1. messages.to_vec() deep-clones all 827 messages every API call** (api.rs:553, api_sync.rs:78,
  openai/mod.rs:184). The CRITICAL long-conversation cost. Fix = `Vec<Arc<Value>>` end-to-end —
  invasive API-layer change, needs a careful dedicated pass + the owned-vs-shared boundary at
  engine/stream.rs:122. → task.
- **A2. name→id session index** — eliminates the residual all-header scans in the resolvers
  (find_session_by_name/set_name) entirely. New persisted artifact (update on save). → task.
- **A3. Session::save re-serializes the entire api_messages every turn** (session.rs:124) — O(N²)
  write growth; this is WHY sessions reach multi-MB. Fix = append-only / incremental session log
  (relates to task #32 unify session storage). → task.
- **A4. render_lines() re-renders all N messages (markdown+syntect) on every cache miss**
  (draw.rs:467) — per-message sub-cache / virtualization. This is task #98. → existing.
- **A5. memory::query_in parses every record in a namespace** (memory/store.rs:155) — unbounded
  growth on a hot RPC. → task.
- **A6. translate.rs messages_to_oai two O(N) passes + per-msg String clone** (translate.rs:113). → task.
- **A7. tools/registry.rs register() rebuilds full schema every call** (O(n²) over tools) — add
  register_many. → task.
- **A8. telemetry/helpers blocking fs writes on the async API path** (helpers.rs:303,
  telemetry.rs:275) — background writer task. → task.
- **A9. events/registry.rs find re-reads every registration file** (same class as latest_session). → task.
