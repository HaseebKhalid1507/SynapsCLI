# Autonomous plugin validation — 2026-09-05

## Result

Implemented the separately installable Python standard-library plugin in
`examples/extensions/autonomous/`. Loop prompts, ordered favorites, successful-turn
limits, duration, retries and cooldown remain outside the Synaps binary. The host
adds only the generic, explicitly user-authorized `session.drive` seam.

First release supports the **local interactive TUI only**. Loading the plugin,
model tool calls, notifications, session resume and process restart cannot arm a
run. Headless chat, RPC, server and worker frontends do not activate drivers.

No plugin installation, PATH binary replacement, live autonomous run or paid
provider smoke test was performed for this feature. All existing uncommitted
changes were preserved; no commit was created. Installed
`/home/jr/.cargo/bin/synaps` remains SHA-256:
`18fd1f6328fd06e876687acd4df2ec464f220666aae36a96f510d00d1611ced5`.

## Final verification

All Rust commands used at most eight compilation workers, with one test thread.

- `cargo test --workspace --locked --offline -j8 --no-fail-fast -- --test-threads=1`:
  **4,124 passed, 0 failed, 34 ignored, 0 filtered**, across 123 reported suites;
  exit 0. Includes four real external-plugin integration tests.
- `cargo clippy --workspace --lib --bins --locked --offline -j8 -- -D warnings`:
  passed, exit 0. This is production-target Clippy, not an all-target lint claim.
- `python3 -m unittest discover -s examples/extensions/autonomous/tests -v`:
  **33 passed**, exit 0.
- `git diff --check`, extension/tool JSON parsing, and
  `bash scripts/loc-ratchet.sh`: passed. The TUI routing spine remains within
  the existing 480-line ceiling; the ceiling was not raised.

Local logs: `/tmp/synaps-autonomous-final.h34KMeiO/`, especially
`workspace2.log` / `.exit`, `clippy-prod2.log` / `.exit`, and
`python2.log` / `.exit`. The earlier pass exposed a two-line routing-spine
ratchet excess and a large task-result enum; both were fixed before the final
full-suite run. Initial all-target cargo check also passed.

## Review findings addressed

- Stop/deadline cancels foreground work and reactive workers, not just the
  foreground token. A pinned originating turn token rejects late worker
  registration even after subsequent user takeover. Registry epochs prevent an
  old deadline sweep from canceling a new turn's workers. Explicit skill-based
  submissions also perform user takeover.
- Revocation inhibits automatic event wakes until new explicit user work or a
  newly armed run. It does not silently reconcile uncollected workers. Raw event
  labels cannot grant an exception.
- Successfully steered/display-only events within the owned in-flight turn do
  not stop the loop. Queued events are classified before further driver work;
  buffered/idle competing work stops the driver.
- Successful same-session durable pressure checkpoints retain the current grant;
  failed saves or changed sessions revoke it. No grant is restored from disk.
- Live process generation is pinned in addition to handler identity. Actual
  child exit, transport loss, shutdown and in-place restart invalidate delayed
  proposals. Child observation is independent of the long-held RPC lock.
- Provider waits are cancellation-aware. Offline loopback tests cover stalled
  headers, error bodies, retry backoff and SSE, including partial content and
  residual usage preservation. Tool dispatch gives cancellation priority and
  distinguishes an unstarted tool from an interrupted side effect.
- Exact model/effort and complete proposed-history checks precede commit, with
  dynamic evidence rechecked after asynchronous preparation. Failure never
  silently aliases a provider or downgrades effort.

## User-visible boundaries

See the [plugin README](../../examples/extensions/autonomous/README.md) for
commands and [session-driver contract](../extensions/session-drivers.md).

- Infinite by default; use `--turns N` and/or `--for 30m` for explicit limits.
  Limits are not inferred from natural-language goal text.
- Typing or **any command, including status**, revokes the grant in this release.
  Escape/Ctrl-C stop while waiting or streaming.
- The requested four defaults are retained verbatim. The current catalog lacks a
  static `anthropic/claude-fable-5-1` row, and `x-ai/grok-4.6` is not the host's
  native `xai-auth` route. Without exact supported evidence those entries are
  visibly rejected/skipped, not silently replaced.
- Cancellation cannot undo external side effects or guarantee provider-side
  billing stops immediately. Durable checkpoint writes remain consistency
  barriers; synchronous work cannot be preempted in the middle of a poll.
  Lifecycle observations are point-in-time checks, not an atomic lease spanning
  plugin process exit and provider dispatch.
- No response-quality, real-account failover or live-model compatibility claim
  is inferred from these offline tests.
