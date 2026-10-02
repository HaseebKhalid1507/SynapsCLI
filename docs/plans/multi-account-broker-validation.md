# Multi-account broker — delivery validation

Date: 2026-09-20. Branch: `feat/multi-account-broker`.
Base: `origin/dev` `7bd33e07` (fast-forward pulled before implementation; recorded in Git reflog).
Final code: `552c62ed` (immutable seat hardening) + `2ca6f123` (lint-only cleanup), following the goal commits listed in [the goal plan](multi-account-broker-goals.md).

## Results

| Check | Result |
|---|---|
| Full workspace, serialized tests | **PASS: 4,671 passed, 0 failed, 40 ignored**, 163 suite summaries; exit 0 |
| `cargo clippy --workspace --lib --bins -- -D warnings` | **PASS** |
| `cargo clippy -p synaps-core --all-targets -- -D warnings` | **PASS** |
| `cargo clippy --all-targets -- -D warnings` (root package / documented CI command) | **PASS** |
| `cargo clippy --workspace --all-targets -- -D warnings` | **Blocked by 14 inherited test-only lints**, listed below; not claimed clean |
| `cargo build --release --bin synaps` | **PASS**, standard repository release profile (LTO, one codegen unit, stripped, panic abort); no release-profile override |
| Release CLI smoke | **PASS**, 15 isolated invocations, empty/synthetic credentials only |
| `git diff --check` | **PASS** |
| Live provider login / real refresh / real activation | **NOT RUN**, pending operator login |

Test command used a fresh HOME under `target/test-homes`, fixed `CARGO_HOME`/`RUSTUP_HOME`, `umask 022`, and unset `SYNAPS_BASE_DIR`, profile/remote-auth/API-key overrides. Some legacy tests set HOME themselves; globally setting `SYNAPS_BASE_DIR` incorrectly defeats their isolation. Build concurrency was 4; dev/test debug info disabled for resource use. PTY tests ran serially:

```bash
cargo test --workspace --no-fail-fast -- --test-threads=1
```

The final run includes account/refresh storage tests, local/remote broker HTTP contracts, all four usage adapters, model-aware routing/failover, keeper fake-clock/loopback tests and subprocess CLI tests. Earlier post-hardening failures were two stale fixture assertions (changed mock identity and compact-vs-pretty JSON comparison); both were corrected and the complete suite rerun successfully.

Local evidence (not committed artifacts):

- `/tmp/synaps-broker-workspace-clean.log` and `.exit` (0)
- `/tmp/synaps-broker-clippy-production.log`
- `/tmp/synaps-broker-clippy-core.log`
- `/tmp/synaps-broker-clippy-root.log`
- `/tmp/synaps-broker-clippy-final.log` (broader workspace test-only blockers)
- `/tmp/synaps-broker-release.log` and `.exit` (0)

## Built artifact and smoke boundary

- Binary: `target/release/synaps` (26,667,736 bytes, mode 0755)
- SHA-256: `31b6d45edd43ddc41de819631319b2576d3c4141b584d7f1d2d2c4676f240a83`
- Smoke used an empty environment with only private HOME/base, PATH and TERM, and a private working directory. It checked version and login/auth/status/keeper help, empty JSON listing/status, no-account keeper refusal, missing-model activation refusal, empty state display, then synthetic account listing, explicit/auto selection and removal. No browser, provider endpoint, real secret or inference was involved.
- No installed executable was replaced, broker restarted, systemd unit enabled, live login attempted, branch pushed or PR created.

## Broader workspace lint blockers (inherited from base)

The expanded `--workspace --all-targets` command includes engine/TUI test targets not linted by the root command. These exact test constructs exist in `7bd33e07`; their surrounding source was checked against the base. The production targets and broker/root test targets pass strict Clippy.

| File (line at check time) | Lint |
|---|---|
| `crates/agent-engine/src/tools/test_helpers.rs:2` | duplicated `cfg(test)` |
| `crates/agent-engine/tests/memory_history_import.rs:512` | needless return |
| `crates/agent-tui/src/tui/models/mod.rs:1988` | manual contains |
| `crates/agent-tui/src/tui/signals.rs:365` | assertion on constants |
| `crates/agent-tui/src/tui/stream_handler.rs:884,890` | find/is_none and find/is_some |
| `crates/agent-engine/src/runtime/api.rs:2744` | single-element loop |
| `crates/agent-engine/src/runtime/google_gemini/setup.rs:654` | test MutexGuard across await |
| `crates/agent-engine/src/runtime/trace/diagnostics.rs:590` | cloned ref to slice |
| `crates/agent-engine/src/runtime/trace/google_wiring_tests.rs:80,637` | type complexity |
| `crates/agent-engine/src/runtime/transport/tests.rs:363` | nonminimal boolean |
| `crates/agent-engine/src/session/wire.rs:1207` | assertion on constants |
| `crates/agent-engine/src/tools/util.rs:79,280` | item after test module |

These were not broadly refactored as part of credential-broker delivery.

## Review and live handoff

Fable workers implemented foundation, usage, routing and keeper slices; an independent Fable worker reviewed security/correctness and followed up. Duplicate identity persistence, corrupt-store behavior, refresh deadlines, inherited-profile handling, remote explicit-default mismatch, canonical keeper lock scope and ledger retention findings were addressed. Later Fable follow-ups were interrupted by provider failures; their saved changes were finished and verified by the foreman. No worker remains running.

Final hardening additionally pairs usage/cache/vended token with immutable seat identity, verifies the full usage and activation identity (not a short display prefix), shares one attempt ledger across aliases, and samples the clock after usage awaits. Tests cover replacement while cached and while an upstream usage read is in flight, removed slots, alias changes and re-login just before activation.

Known limitations are explicit in the [login guide](../providers/multi-account-broker.md) and [keeper guide](../providers/quota-keeper.md): cancellation during rotating refresh may still require re-login; cooldowns are process-local; not every provider exposes deduplication identity; Grok billing is not live-verified; first-use reset anchoring is not universally established; activation model must match the desired quota bucket; no guaranteed output-token ceiling exists on the Codex endpoint. Separate banked-reset inventory fetch is supported by the adapter but not forced by normal polling; missing expiry is shown as unknown, never invented.

Next operational step: use the built binary on the broker host to log into two Codex and two Claude slots, run `auth list` and `status --all --json`, then a read-only `quota-keeper --once`. Only after checking actual reset/bucket evidence should the operator opt accounts into activation. The feature is ready for that login pass; production reset activation has not been claimed successful.
