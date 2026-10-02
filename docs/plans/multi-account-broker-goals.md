# Goal plan: multi-account OAuth broker and quota keeper

Status: G1–G7 implementation and automated validation complete; live account validation pending operator login. Owner: foreground foreman; implementation/review workers: `anthropic/claude-fable-5-1`.
Base: `origin/dev` `7bd33e07`; branch `feat/multi-account-broker`. Companion: `multi-account-credential-broker.md` (initial design, superseded by safety details below).

## Outcome

One Synaps broker manages multiple legitimate Claude, ChatGPT/Codex, Kimi Code and Grok OAuth accounts, refreshes each independently, reports real quota/reset information, and offers explicit opt-in capacity selection and Codex weekly-window activation. Existing single-account installations keep working without migration. No passwords/refresh tokens reach dashboards, logs or model runtimes.

## Decisions and constraints

- Additive `provider@label` storage keys; bare provider remains `default`. Validate labels at every boundary. Explicit account requests never silently fall back to another account.
- Automatic selection is opt-in. Unknown/stale/malformed quota is **not** available capacity. Account-specific tokens, headers, caches and cooldowns must stay paired.
- Refresh single-flight includes the resolved credential file and account, not just provider. File locking must protect refresh rotation across processes as well as atomic persistence; do not mutate global profile/environment to choose an account.
- Broker remains the only refresh-token owner. Login into a named slot must never transiently overwrite default. Preserve unrelated keys/metadata and handle removals/re-login during refresh safely.
- Duplicate provider identity under another label must not become two independent refresh owners. Reject without overwriting credentials; providers lacking a trustworthy identity report that limitation. Never log JWTs or upstream error bodies.
- Machine-token clients already have provider credential access. Account listing exposes labels and minimal identity metadata, not credentials; no unauthenticated identity listing. Keep strict proxy endpoint allowlists and no-redirect credential-bearing HTTP.
- Distinguish quota reset, OAuth expiry, billing renewal and activation. Do not hardcode `primary = 5h`: real Codex accounts can have a weekly primary and no secondary.
- The earlier research establishes first-use anchoring for some reset paths, not a universal documented guarantee for every natural rollover. Provider-reported timestamps are authoritative. Activation is tracked and verified, not inferred from `now + 7d` alone.
- Keeper is read-only by default. Any inference requires explicit per-account opt-in and a bounded, tool-free request on the correct provider/account/model. No automatic purchase, banked-reset redemption, account creation, token copying from other CLIs or login.
- Never replay a request on a second account after partial output/tool activity. One bounded Codex failover only on a recognized pre-output quota failure; ordinary 429s are not proof of weekly exhaustion.
- User will perform real multi-account Claude/ChatGPT logins when the built, tested commands are ready. Tests use temporary directories and mock transports; do not touch `~/.synaps-cli/auth.json` or refresh real credentials.

## Goals, evidence and dependencies

### G1 — Multiple credentials are safe and independently addressable
- [x] Validated account reference and backwards-compatible storage/list/remove APIs.
- [x] Account-aware refresh with same-account serialization and cross-account isolation.
- [x] Login accepts a named destination for every OAuth flow without a default-slot write.
- [x] Regression tests: legacy files, malformed labels/files, preservation, duplicate identity, concurrent refresh, no-secret summaries.
**Done when:** two mock Codex/Claude accounts coexist and refreshing/removing one cannot affect the other.

### G2 — Local and remote clients select the same account consistently
Depends on G1.
- [x] Account-aware broker token/proxy/usage/capability APIs and machine authentication.
- [x] Explicit per-call selection, provider config/environment selection, default compatibility.
- [x] Remote cache keys include source/provider/account; account-specific Codex header is derived from the same credential as its bearer token.
- [x] Broker health/startup works for non-Anthropic-only installations.
**Done when:** mock local and HTTP broker tests select A/B independently and reject unknown accounts without fallback.

### G3 — Real quota information is normalized for all four providers
Parallel with G1; integrate after G2.
- [x] Typed, sanitized usage snapshot with observation time, dynamic windows, resets, model availability and optional banked-reset inventory.
- [x] Read-only pinned adapters: Anthropic OAuth usage; Codex wham usage; Kimi Code usages; Grok Build billing.
- [x] Missing/invalid percentages, unsupported sources, stale readings and auth failures remain explicitly unknown/error.
- [x] Fixture-based parsers and fake-server tests; usage calls do not generate inference.
**Done when:** `synaps status --all --json` reports every connected supported account (including errors without discarding successful accounts).

### G4 — Operator can add, inspect, choose and remove accounts
Depends on G1–G3.
- [x] `synaps login --provider <id> --account <label>` (and `auth login` alias).
- [x] `synaps auth list`, `auth use`, `auth remove`, with JSON listing and explicit removal confirmation.
- [x] `synaps status` provider/account/all/JSON flags preserve existing memory-status behavior.
- [x] CLI parsing/help, no-login mock smoke tests and practical login runbook.
**Done when:** user can connect multiple Claude and Codex subscriptions without hand-editing auth.json.

### G5 — Optional routing uses proven capacity and bounded failover
Depends on G2/G3.
- [x] Explicit `auto` policy, fresh snapshot selection, model-relevant limits and account cooldowns.
- [x] Account pinning for a request; no token/header identity race; no exhausted/unknown account advertised as ready.
- [x] One safe Codex failover before any output; no cross-account replay after output.
- [x] Tests for exhausted/stale/no-capacity/all-failed and retry bounds.
**Done when:** mock quota exhaustion can choose a different eligible seat without looping or replaying work.

### G6 — Quota keeper prevents avoidable activation delay
Depends on G2–G5.
- [x] Persistent scheduler state keyed by source/provider/account/reset generation; private atomic state and exclusive singleton/attempt locking.
- [x] Poll-only dry-run default; explicit per-account activation opt-in; once/daemon modes; bounded polling/backoff and HTTP timeouts.
- [x] States distinguish exhausted, due/awaiting activation, activation pending verification, active, unknown/auth error. Record idle delay and errors.
- [x] Minimal Codex inference has no project context, tools, MCP, shell or agent loop; success requires fresh provider evidence of a new active window. Crash/timeout ambiguity never causes repeated token burning.
- [x] Banked reset inventory/expiry alerts where exposed; no purchase/redemption.
- [x] Optional systemd user-service example, private state, graceful termination and operator runbook (disabled by default).
**Done when:** deterministic fake-clock/mock-server tests show one activation per eligible rollover, no activation while exhausted/stale/unauthorized, and recovery after restart without duplicate spending.

### G7 — Review, validate and deliver a usable build
Depends on all above.
- [x] Independent security/correctness review by Fable worker, findings fixed or explicitly documented.
- [x] Targeted tests, workspace test attempt (PTY serialized on retry), clippy/build checks with exact blockers distinguished from regressions.
- [x] Release binary built and CLI help/dry-run smoke verified in isolated configuration; do not replace a live running installation or enable spending services without an explicit operational step.
- [x] Update goal checkboxes with evidence; operator commands for live login and final smoke. Commit changes on feature branch; no push/PR unless requested.
**Done when:** code and automated validation are complete, or an external blocker is recorded precisely. Live account validation is explicitly pending user login, not falsely marked passed.

## Execution assignments

1. **Foundation worker:** G1/G2; owns auth storage/refresh/login flows, broker trait/transport, account config. Defines concrete public interfaces before dependent integrations.
2. **Usage worker:** G3 normalized snapshots, parsers and read-only fetch helpers in a separate module; no shared broker edits until integration assigned.
3. **Keeper/policy worker:** pure scheduler/capacity state + tests in separate modules for G5/G6; integrates with broker/CLI after foundation interfaces stabilize.
4. **CLI/runtime worker (second wave):** G4 and G5 engine integration, consumes landed interfaces.
5. **Reviewer (final wave):** G7 read-only audit with specific file/line findings and targeted tests.
6. **Foreman:** review contracts, integrate non-overlapping changes, run complete validation, supervise every handle to terminal status and reconcile, correct documentation, deliver login instructions.

Workers must not push, access live secrets, execute login/inference, enable services or install over the running binary. Bounded task timeouts are progress checkpoints, not end-of-task permission: resume unfinished workers as needed.


## Implementation evidence

- **G1/G2:** `auth/{account,storage,token,broker,credential_source}.rs`, account-aware provider login flows and `src/cmd/auth_broker.rs`; fixture/integration tests in `crates/agent-core/tests/broker_accounts.rs`. Commits `07e13417`, `4857ed23`, `86d8b7c0` plus final seat-identity hardening. Named-only login availability is an inventory/UI answer, not proof of routable capacity; explicit named selection still never falls back.
- **G3:** `auth/usage.rs`, `src/cmd/status.rs`, [usage schema/runbook](../providers/multi-account-usage.md); commit `52c25d6c`. All four adapters use bounded read-only requests. Live Grok billing schema remains unverified. Codex snapshot identity is paired to the bearer and carries an opaque full-seat fingerprint.
- **G4:** `src/cmd/{login,auth_accounts,status}.rs`, `src/main.rs`, `tests/multi_account_cli.rs`; [operator login runbook](../providers/multi-account-broker.md). Subprocess tests use empty environments/private synthetic stores and never open a browser or call a real provider.
- **G5:** pure `auth/quota_policy.rs`, broker capacity cache/cooldowns, `runtime/openai/account_routing.rs` and stream integration; commits `392a9487`, `95ab4299`, `b03bdb55`. Regression cases include cached/in-flight usage while a label is re-logged into another seat, removed slots, explicit/default remote pairing, model limits, no replay after output and one bounded pre-output Codex failover.
- **G6:** pure `auth/quota_keeper.rs`, `src/cmd/quota_keeper.rs`, [keeper runbook](../providers/quota-keeper.md), disabled `deploy/synaps-quota-keeper.service`; commit `b03bdb55` plus final alias-independent ledger and pre-send full-seat checks. Clock evaluated after fetch; full usage identity also checked so a matching short prefix cannot authorize or falsely verify another seat. Deterministic tests cover restart ambiguity, once-per-generation budget, aliases, per-account opt-in, remote refusal, canonical ledger, model mismatch and re-login.
- **Review:** independent Fable audit and follow-up (workers sa5/sa6) recorded duplicate-login RMW, refresh deadlines, corrupt-store handling, profile fallback, remote-default mismatch and canonical keeper locks; fixed in implementation. Foreman reviewed subsequent seat/cache/keeper changes and added adversarial regressions. See final validation record for remaining inherited limitations and exact check results.

Checked goals mean implementation + automated evidence, **not production-provider certification**. No installed binary has been replaced, no live credential has been read/refreshed by validation, no activation service has been enabled, and no feature branch has been pushed. User login and a deliberate first live quota/reset observation remain the operational handoff.

Final evidence: [multi-account-broker-validation.md](multi-account-broker-validation.md). Full workspace: **4,671 passed / 0 failed / 40 ignored**. Standard release build and isolated CLI smoke passed. Root/core all-targets and workspace production Clippy passed; broader workspace all-targets has 14 documented inherited test-only lints. Final code commits: `552c62ed`, `2ca6f123`.
