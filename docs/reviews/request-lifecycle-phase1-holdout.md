# Phase 1 Holdout Verdict — FINAL

- **Verdict:** PASS
- **Weighted total:** 0.903
- **Commit range:** `d20e03f..97374e6` (worktree clean; reviewed at `97374e6`)
- **Reviewer:** independent holdout oracle (kimi-k3); scratch `/tmp/phase1-kimi-k3-holdout/`; no repo files modified.

## Per-axis scores

| Axis | Weight | Score | Evidence |
|---|---|---|---|
| security/privacy | 0.35 | 0.90 | probes P1–P8 below; `core/error.rs:32-110`, `runtime/api.rs:558-594,1128-1154`, `openai/stream.rs:68-90,187-198`, `openai/wire.rs:144-152,193-199`, `auth/broker.rs:1792-1804,1829-1838,1947-1963,2004-2011,2030-2037`; finding F1 (low) |
| correctness | 0.30 | 0.92 | required runs all green (appendix); `engine/stream.rs:89-133` (baseline repair), `src/cmd/chat.rs:461-508` (nonzero exit), TUI/RPC/server typed forwarding |
| spec fidelity | 0.20 | 0.90 | §5.1–§5.5 all independently verified; sync-path compaction verified by source audit + unit tests only (L1); `private_fs.rs:93-172`, `text.rs:23-35`, `stream_types.rs:89-107` |
| code quality | 0.10 | 0.88 | single-derivation `turn_error_for` (`runtime/helpers.rs:50-78`); vetted-static-label design; dropped-but-kept `MAX_UPSTREAM_ERROR_BYTES` is dead-weight (F2) |
| docs | 0.05 | 0.90 | spec §4/§5 accurate; README.md:100 text-only claim matches enforced behavior; plan T1–T6 traceable |

Weighted: 0.35×0.90 + 0.30×0.92 + 0.20×0.90 + 0.10×0.88 + 0.05×0.90 = 0.315+0.276+0.180+0.088+0.045 = **0.904** (rounding 0.903 with conservative axis rounding).

## Independent probe evidence (my own scratch servers, not implementation tests)

Built `hostile_anthropic.py` / `hostile_oai.py` + drivers in `/tmp/phase1-kimi-k3-holdout/`; each run used an isolated HOME, synthetic OAuth creds, `RUST_LOG=trace`, sentinel `K3-HOLDOUT-SENTINEL-8c2b1e` in the user message (and system prompt for OAI), and scanned stdout, stderr, `synaps.log*` for sentinel / `ECHOED:` / `input_schema` / `"messages"`:

| Probe | Attack | Result |
|---|---|---|
| P1 anthropic echo500 | 500 JSON `error.message = "ECHOED:"+full request body` | NO-LEAK; exit 1; stderr shows only `Anthropic server error (HTTP 500 [api_error]). Retries exhausted…` |
| P2 anthropic ansi500 | 500 text/plain + ANSI escapes + raw body | NO-LEAK; no provider ANSI in any sink; exit 1 |
| P3 anthropic midstream | 200 SSE, partial text, then SSE `error` event with unvetted type + echoed body | NO-LEAK; surfaced `API stream error (unrecognized_error). Provider error details withheld`; exit 1 |
| P4 anthropic sse_unknown | unknown SSE event type carrying echoed body | NO-LEAK; logs carry only byte counts |
| P5 anthropic echo400 | 400 with attacker `error.type` containing sentinel | NO-LEAK; vetted-type allowlist holds; exit 1 |
| P6 anthropic echo429 | 429 + sentinel in `request-id`/reset headers + echoed body | NO-LEAK; 8 retries then static exhausted message; exit 1 |
| P7 OAI echo500 (`local/m` via LocalBroker) | 500 `{"error":{"message":"ECHOED:"+body}}` | NO-LEAK; `openai request failed: broker transport error: provider request failed: 500 Internal Server Error`; exit 1 |
| P8 OAI ssebad + ssefinish | malformed SSE data line w/ echoed body; unknown `finish_reason` = sentinel | NO-LEAK; log shows `sse parse error: … (payload 2012 bytes, not logged)` and `unknown finish_reason (value not logged)` |

Binary-level partial-history check: every failing run still saved a session file containing the sentinel user message (valid JSON), matching §5.2.

Source-audit confirmations (no unredacted residual path found):
- Anthropic retry state keeps only `format!("HTTP {}", status)` (`api.rs:1154`, `api_sync.rs:261,527`); terminal error goes through `humanize_api_error_with_reset`, which emits static guidance + status + allow-listed `[error.type]` only, using the body solely for fixed-pattern classification (`core/error.rs:54-110`).
- Anthropic SSE error/unknown arms emit vetted static labels + `frame_bytes`/`payload_bytes` only (`api.rs:558-594`); in-stream retry warns reuse that same static message.
- OpenAI-compatible `send_with_retries` drops the body unread (`openai/stream.rs:71-90`); broker-open errors pass `redact_provider_proxy_error` (`openai/stream.rs:190-198,1631-1636`; `net.rs:68-87`), which is belt-and-suspenders since `LocalBroker`/`RemoteBroker` now drop all error bodies unread (`broker.rs:1795-1804, 1832-1838, 1950-1962, 2007-2011, 2033-2037`).
- Gemini runtime redacts broker-flattened snippets at both stream-open and mid-stream (`google_gemini/runtime.rs:257-269,354-362`); vertex surfaces typed static errors only (`google_vertex.rs:28-36`).

## Findings

| # | Severity | Location | Description | Repro/impact |
|---|---|---|---|---|
| F1 | Low | `crates/agent-engine/src/runtime/mod.rs:72` | Extension-hook confirm messages are logged verbatim at INFO (`message = %message`). Hook messages are extension-authored, not provider-controlled, and can embed tool args; spec §5.1 excludes tool arguments "by default". Outside the 97374e6 blast radius but within §5.1 scope. | An extension emitting `Confirm { message }` with tool-call detail lands in `synaps.log*` at default levels. |
| F2 | Low (hygiene) | `crates/agent-core/src/core/auth/broker.rs:50-53` | `MAX_UPSTREAM_ERROR_BYTES` retained "only because re-exported from auth::mod" — dead weight inviting future misuse. | none (code quality) |
| F3 | Info | `runtime/api.rs:1128`, `api_sync.rs:239,505` | Hostile bodies are still read into memory unbounded before discard/classification (`resp.text()`), and broker/usage success paths use `read_body_capped`. Classification-only; no leak. A memory-flood (not disclosure) consideration. | provider returns giant error body → transient allocation; pre-existing pattern. |

## §5.2–§5.5 + PR #63 verification

- **TurnOutcome typed everywhere:** enum + correlation IDs at `agent-core/src/core/stream_types.rs:77-158`; single derivation `turn_error_for` (`runtime/helpers.rs:50-78`); headless exits nonzero after saving partial history (`src/cmd/chat.rs:461-508`, probe exit codes = 1); RPC embeds typed `outcome` (`src/cmd/rpc.rs:395`); server/TUI/subagent forward category + correlation ID (`src/cmd/server.rs:772-779,1108-1117`; `crates/agent-tui/src/tui/stream_handler.rs:187-193`; `tools/subagent/{oneshot,resume,start}.rs`). Baseline-index history repair never removes pre-existing trailing messages (`engine/stream.rs:89-133`, unit-tested).
- **BoundedText:** byte-budget at char boundary (`agent-core/src/text.rs:23-35`, `truncate_str` in `lib.rs`); budget 0 → `end` floored at 0 (no panic, empty); sub-codepoint budgets back off greedily. Migrated paths: `truncate_tool_result` (`runtime/helpers.rs:306-318`), `tools/grep.rs:81-88`, `tools/bash.rs:280`. Remaining `chars().take()` sites (subagent previews, `mcp/lazy.rs:117`) are char-budgeted previews that cannot slice mid-codepoint — not byte-slicing defects.
- **Private FS:** `private_fs.rs:70-172`: `ensure_private_dir` (0700 + repair), `open_private_append` (0600 create, `O_NOFOLLOW`, `ELOOP`→typed `SymlinkRefused`, fchmod repair on open handle), `write_atomic_private` (create_new 0600 temp, symlink re-check, rename — rename replaces rather than follows). Parent-component symlinks are safely neutralized by design (base dirs are `ensure_private_dir`-ed to 0700; rename targets the dir entry, not the link). Unit tests + binary-level umask-000 runs green.
- **Cloud text-only:** `supports_tools: false` in all 3 descriptors (`auth/cloud.rs:288-313`); `preflight_cloud_capability` → typed `BrokerError::UnsupportedCapability` (`broker.rs:2191-2202`) raised in `call_api_stream_inner` before broker construction/credentials/network (`runtime/api.rs:796-806`); invoke-time `Denied` guard remains (`broker.rs:956-960`); README.md:100 documents text-only.
- **PR #63 invariants:** Phase 1 diff does not touch `orchestration.rs` (0 lines) or `authorize_model.rs`; subagent tool diffs are error-label-only. Exact-model parse→resolve→authorize flow (`orchestration.rs:269-330`) with `network_attempted: false` denials, session-scoped `grant_worker_model`, and baseline foreground-only catalog confirmed intact at HEAD.

## Required runs (exact outcomes)

| Command | Outcome |
|---|---|
| `cargo test --test phase1_privacy` | **ok. 9 passed; 0 failed; 2 ignored** (worker-entry tests, by design) |
| `cargo test -p synaps-engine --lib` | **ok. 1257 passed; 0 failed** |
| `cargo test -p synaps-core` | **ok. 385 passed; 0 failed** (lib) + all integration suites ok |
| `cargo check --workspace` | Finished clean, no warnings |
| `git diff --check` | clean (no output) |
| extras: `cargo test --workspace` | 80 suites, all `ok`, 0 failures — no flaky tests observed across 3 consecutive runs |
| extras: `cargo test --test chat_stdin --test c6_cross_mode_contract` | 5 + 7 passed, 0 failed |

## Residual risks / limitations

- **L1 (main):** the Anthropic **sync/non-streaming** path (`api_sync.rs`, used by `run_single` and `/compact`) hardcodes `https://api.anthropic.com` (no base-URL override), so I could not drive it against my loopback hostile server without prohibited system mutation. Coverage rests on (a) line-level audit showing identical vetted-static redaction as the streaming path (`api_sync.rs:191-200,250,261,462-465,516,527`, incl. removal of the old `eprintln!("API Error Response: {pretty json}")`), and (b) implementation unit tests. Risk: low — the three body-touching sites all terminate in `humanize_api_error_with_reset`/`sanitize_error_type`, which my binary-level probes exercised end-to-end via the streaming path.
- SSE mid-stream truncation (connection drop) surfaces `humanize_network_error` static text — verified by audit, not probed.
- Gemini/Vertex probed via audit + their in-crate hostile-broker unit tests; my binary probes covered Anthropic + OpenAI-compatible transports directly.
- F1 extension-confirm log content is extension-controlled; threat model assumes local extensions are trusted-ish, but it is a §5.1 "tool arguments by default" gray zone.

## Ordered required fixes
None blocking. Suggested follow-ups: (1) log extension confirm messages metadata-only (F1); (2) drop `MAX_UPSTREAM_ERROR_BYTES` re-export (F2); (3) consider a test-only base-URL seam for `api_sync.rs` to enable loopback probing (L1).
