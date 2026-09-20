# Goal plan: multi-account OAuth broker and quota keeper

Status: execution in progress. Owner: foreground foreman; implementation/review workers: `anthropic/claude-fable-5-1`.
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
- [ ] Validated account reference and backwards-compatible storage/list/remove APIs.
- [ ] Account-aware refresh with same-account serialization and cross-account isolation.
- [ ] Login accepts a named destination for every OAuth flow without a default-slot write.
- [ ] Regression tests: legacy files, malformed labels/files, preservation, duplicate identity, concurrent refresh, no-secret summaries.
**Done when:** two mock Codex/Claude accounts coexist and refreshing/removing one cannot affect the other.

### G2 — Local and remote clients select the same account consistently
Depends on G1.
- [ ] Account-aware broker token/proxy/usage/capability APIs and machine authentication.
- [ ] Explicit per-call selection, provider config/environment selection, default compatibility.
- [ ] Remote cache keys include source/provider/account; account-specific Codex header is derived from the same credential as its bearer token.
- [ ] Broker health/startup works for non-Anthropic-only installations.
**Done when:** mock local and HTTP broker tests select A/B independently and reject unknown accounts without fallback.

### G3 — Real quota information is normalized for all four providers
Parallel with G1; integrate after G2.
- [ ] Typed, sanitized usage snapshot with observation time, dynamic windows, resets, model availability and optional banked-reset inventory.
- [ ] Read-only pinned adapters: Anthropic OAuth usage; Codex wham usage; Kimi Code usages; Grok Build billing.
- [ ] Missing/invalid percentages, unsupported sources, stale readings and auth failures remain explicitly unknown/error.
- [ ] Fixture-based parsers and fake-server tests; usage calls do not generate inference.
**Done when:** `synaps status --all --json` reports every connected supported account (including errors without discarding successful accounts).

### G4 — Operator can add, inspect, choose and remove accounts
Depends on G1–G3.
- [ ] `synaps login --provider <id> --account <label>` (and `auth login` alias).
- [ ] `synaps auth list`, `auth use`, `auth remove`, with JSON listing and explicit removal confirmation.
- [ ] `synaps status` provider/account/all/JSON flags preserve existing memory-status behavior.
- [ ] CLI parsing/help, no-login mock smoke tests and practical login runbook.
**Done when:** user can connect multiple Claude and Codex subscriptions without hand-editing auth.json.

### G5 — Optional routing uses proven capacity and bounded failover
Depends on G2/G3.
- [ ] Explicit `auto` policy, fresh snapshot selection, model-relevant limits and account cooldowns.
- [ ] Account pinning for a request; no token/header identity race; no exhausted/unknown account advertised as ready.
- [ ] One safe Codex failover before any output; no cross-account replay after output.
- [ ] Tests for exhausted/stale/no-capacity/all-failed and retry bounds.
**Done when:** mock quota exhaustion can choose a different eligible seat without looping or replaying work.

### G6 — Quota keeper prevents avoidable activation delay
Depends on G2–G5.
- [ ] Persistent scheduler state keyed by source/provider/account/reset generation; private atomic state and exclusive singleton/attempt locking.
- [ ] Poll-only dry-run default; explicit per-account activation opt-in; once/daemon modes; bounded polling/backoff and HTTP timeouts.
- [ ] States distinguish exhausted, due/awaiting activation, activation pending verification, active, unknown/auth error. Record idle delay and errors.
- [ ] Minimal Codex inference has no project context, tools, MCP, shell or agent loop; success requires fresh provider evidence of a new active window. Crash/timeout ambiguity never causes repeated token burning.
- [ ] Banked reset inventory/expiry alerts where exposed; no purchase/redemption.
- [ ] Optional systemd user-service example, private state, graceful termination and operator runbook (disabled by default).
**Done when:** deterministic fake-clock/mock-server tests show one activation per eligible rollover, no activation while exhausted/stale/unauthorized, and recovery after restart without duplicate spending.

### G7 — Review, validate and deliver a usable build
Depends on all above.
- [ ] Independent security/correctness review by Fable worker, findings fixed or explicitly documented.
- [ ] Targeted tests, workspace test attempt (PTY serialized on retry), clippy/build checks with exact blockers distinguished from regressions.
- [ ] Release binary built and CLI help/dry-run smoke verified in isolated configuration; do not replace a live running installation or enable spending services without an explicit operational step.
- [ ] Update goal checkboxes with evidence; operator commands for live login and final smoke. Commit changes on feature branch; no push/PR unless requested.
**Done when:** code and automated validation are complete, or an external blocker is recorded precisely. Live account validation is explicitly pending user login, not falsely marked passed.

## Execution assignments

1. **Foundation worker:** G1/G2; owns auth storage/refresh/login flows, broker trait/transport, account config. Defines concrete public interfaces before dependent integrations.
2. **Usage worker:** G3 normalized snapshots, parsers and read-only fetch helpers in a separate module; no shared broker edits until integration assigned.
3. **Keeper/policy worker:** pure scheduler/capacity state + tests in separate modules for G5/G6; integrates with broker/CLI after foundation interfaces stabilize.
4. **CLI/runtime worker (second wave):** G4 and G5 engine integration, consumes landed interfaces.
5. **Reviewer (final wave):** G7 read-only audit with specific file/line findings and targeted tests.
6. **Foreman:** review contracts, integrate non-overlapping changes, run complete validation, supervise every handle to terminal status and reconcile, correct documentation, deliver login instructions.

Workers must not push, access live secrets, execute login/inference, enable services or install over the running binary. Bounded task timeouts are progress checkpoints, not end-of-task permission: resume unfinished workers as needed.
