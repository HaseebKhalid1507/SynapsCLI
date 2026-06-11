# Plan: Performance & Structure Fixes — S205 continuation

## Pre-plan findings (recon)
- P7 (tool registry clone): ALREADY CHEAP — `cached_schema` is `Arc<Vec<Value>>`, snapshot clone is pointer bumps + small String maps. Skip.
- P3 (Arc message history): launch-time clone is SEMANTICALLY REQUIRED — TUI renders history while runtime extends its own copy. `Arc::make_mut` copies on first append anyway (runtime always appends). The api.rs `to_vec` feeds body construction which needs owned values. Close as analyzed, document in REVIEW.md.
- SSE parser has ZERO tests. Any parsing change needs the safety net first.

## Task 1: Extract SseLineBuffer + chunk-boundary tests (P2a + A5)
**Description:** Extract the byte-buffer/line-splitting logic from `call_api_stream_inner` into a testable `SseLineBuffer` struct. Zero-copy line access (borrow from buffer, no to_vec/to_string double copy). memchr for newline search. Then chunk-boundary tests: lines split mid-byte, mid-UTF-8, CRLF, [DONE], empty lines.
**Acceptance:**
- [ ] No `to_vec()` or `from_utf8_lossy().to_string()` per line in the hot loop
- [ ] Chunk-boundary tests: split mid-line, mid-UTF-8 char, CRLF endings, keepalive comments
- [ ] Same events emitted as before (existing 906 tests green)
**Verification:** `cargo test --lib runtime::` + full suite
**Files:** src/runtime/api.rs (new module src/runtime/sse.rs)
**Scope:** M | **Dependencies:** None | **Risk:** high-value, do first

## Task 2: draw.rs per-frame alloc fixes (P4 + P9)
**Description:** (a) version string formatted per frame → const; (b) per-char `ch.to_string()` art spans → grouped same-style runs; (c) selection overlay `cell.symbol().to_string()` per cell → in-place style mutation; (d) progress-bar `"█".repeat()` per frame → only when value changed (skip if invasive).
**Acceptance:**
- [ ] No `format!` of compile-time constants in draw path
- [ ] No per-char String allocation in art rendering
- [ ] Selection overlay mutates styles without extracting symbols
**Verification:** `cargo test --lib`, visual check in debug binary pane
**Files:** src/tui/draw.rs
**Scope:** S | **Dependencies:** None

## Task 3: Build-speed quick wins (D1-D3 from A1/A2)
**Description:** (a) dev profile: `debug = "line-tables-only"` (Linux); (b) run cargo machete + cargo tree -d, remove unused/duplicate deps if found; (c) check reqwest/tokio feature flags for double-TLS or unused features.
**Acceptance:**
- [ ] Dev profile tuned, incremental check measurably faster or no regression
- [ ] No unused deps remain (or documented why kept)
- [ ] reqwest pulls exactly one TLS stack
**Verification:** `cargo build --timings` before/after, `cargo test --lib`
**Files:** Cargo.toml
**Scope:** XS-S | **Dependencies:** None

## Task 4: Document P3/P7 closure + update REVIEW.md
**Description:** Mark fixed items in REVIEW.md, document why P3/P7 are closed-as-analyzed.
**Scope:** XS | **Dependencies:** Tasks 1-3

## Checkpoint after Task 1: full test suite + debug binary smoke test in pane
## Checkpoint after Task 3: cargo build --timings comparison

## Deferred (next session, own context):
- Typed SSE events (P2b) — needs Task 1's tests as foundation; Map-alloc win is smaller than line-copy win
- Workspace split (A3) — 3-5 day job
- process.rs split (B1) — 2446 lines, needs focused review
- Cache strategy single-last switch — handoff item, needs benchmark re-run
