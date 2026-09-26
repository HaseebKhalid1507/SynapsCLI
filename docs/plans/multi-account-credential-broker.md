# Plan: Multiple accounts per OAuth provider in the credential broker

**Status:** draft for JR review · **Origin:** quota-juggling research 2026-09-20 (several ChatGPT/Codex "Astra" seats hit their weekly cap in ~24h; the broker can only hold one refresh token per provider) · **Branch:** `feat/multi-account-broker` off `dev @ 7bd33e07`

> Execution update: the complete, goal-based scope and safety acceptance criteria are in [multi-account-broker-goals.md](multi-account-broker-goals.md). That plan supersedes draft details below, particularly cross-process refresh serialization, source/principal-scoped token caches, fail-closed invalid selectors, and bounded opt-in activation with provider verification. The initial Phase A-only PR scope is no longer the full task.

## Problem

`auth.json` is a flat map keyed by provider id (`"openai-codex"`, `"anthropic"`, `"kimi-code"`, `"xai-auth"`, …). Every layer above it — `load_provider_auth(key)`, the per-provider refresh gate, `CredentialBroker::access_token(OAuthProviderId)`, `GET /token?provider=X` — assumes **exactly one credential per provider**. Holding a second ChatGPT seat today means a second profile directory (`~/.synaps-cli/<profile>/auth.json`) and a second broker process, and nothing can rotate between them.

What we need:

1. One broker, one `auth.json`, **N refresh tokens per provider**, each refreshed independently by the single refresher (rotating refresh tokens still mean exactly one party may refresh a given token — that invariant stays).
2. Explicit selection ("use the `astra2` Codex account") from config, env, CLI, or per HTTP request.
3. Later: automatic selection ("give me whichever Codex account has weekly headroom") and rotate-on-limit — that is the "juggling", and it needs usage data the broker can already fetch token-free (`chatgpt.com/backend-api/wham/usage`, `api.anthropic.com/api/oauth/usage`, `api.kimi.com/coding/v1/usages`, `cli-chat-proxy.grok.com/v1/billing`).

## Principle

**An account is a `(provider, label)` pair; the label is part of the storage key, and the bare provider key is the `default` label.** Nothing that exists today moves, changes shape, or needs migration. Every API that takes a provider grows an optional account, defaulting to the selected account for that provider, so existing call sites compile and behave identically.

## Design

### Storage (`auth.json`) — additive, no migration

```jsonc
{
  "openai-codex":        { "type": "oauth", "refresh": "…", "access": "…", "expires": 0, "accountId": "2b2f…" },
  "openai-codex@astra2": { "type": "oauth", "refresh": "…", "access": "…", "expires": 0, "accountId": "7f53…",
                           "label": "astra2", "identity": "inference@praxis-ai.com", "addedAt": 1789911074 },
  "anthropic":           { … },
  "kimi-code@m27":       { … }
}
```

- Storage key: `<provider>` for the default account, `<provider>@<label>` otherwise.
- Label grammar: `^[a-z0-9][a-z0-9._-]{0,31}$`. `default` is reserved and means the bare key. Rejected labels never reach the filesystem or a URL.
- `label`, `identity`, `addedAt` are optional non-secret metadata on `OAuthCredentials` (`#[serde(default, skip_serializing_if)]`). `identity` is whatever the provider login can cheaply tell us (Codex: email claim from the id_token; Anthropic: none today) and is display-only.
- `save_provider_fields_at` (fs4 lock + tmp/rename merge) is already key-agnostic — reused unchanged.
- `AuthFile` (typed struct requiring `anthropic`) is only used by the broker's startup check and `/healthz`; both switch to "at least one OAuth credential of any provider" so a Kimi-only or Codex-only broker can start (today `m27-kimi` cannot).

### Types (`crates/agent-core/src/core/auth/account.rs`, new)

```rust
pub enum Account { Default, Named(AccountLabel) }        // AccountLabel = validated newtype
pub struct CredentialRef { pub provider: OAuthProviderId, pub account: Account }
impl CredentialRef {
    pub fn storage_key(&self) -> String;                  // "openai-codex" | "openai-codex@astra2"
    pub fn parse(key: &str) -> Option<Self>;              // inverse; None for static-key/cloud keys
}
pub enum AccountSelector { Named(Account), Auto }         // what a caller *asks for*
pub struct AccountPolicy { per_provider: BTreeMap<OAuthProviderId, AccountSelector>, order: … }
```

`list_accounts(provider) -> Vec<(Account, AccountSummary)>` enumerates `auth.json` keys; `AccountSummary` carries label, identity, `account_id` prefix, expiry, `configured` — never token material.

### Refresh (`token.rs`)

`ensure_fresh_provider_token(client, provider)` becomes a wrapper over
`ensure_fresh_credential(client, &CredentialRef)`. The single-flight gate registry is already keyed by string; it is keyed by `storage_key()`, so two accounts of the same provider refresh concurrently and independently while one account can never double-rotate. The load/save closures use the storage key. No change to `ensure_fresh_gated`.

### Broker trait (`broker.rs`)

```rust
async fn access_token(&self, provider: OAuthProviderId) -> Result<AccessToken, BrokerError> {
    self.access_token_for(&self.policy().resolve(provider)).await        // default impl
}
async fn access_token_for(&self, cred: &CredentialRef) -> Result<AccessToken, BrokerError>;
async fn accounts(&self, provider: OAuthProviderId) -> Result<Vec<AccountSummary>, BrokerError>;
```

- `LocalBroker` and `RemoteBroker` hold an `AccountPolicy` built at construction (`broker_from_source(source, cache, http)` gains the policy from config; the global broker installs it once).
- `AccessToken` gains `account: Account` so callers (Codex stream) can log which seat served a request.
- `LocalBroker::send` for `openai-codex` currently re-reads `load_provider_auth("openai-codex")` for the `chatgpt-account-id` header; it uses the resolved `CredentialRef` instead (JWT-claim fallback unchanged).
- `ProxyRequest` gains `account: Option<String>` (validated as a label; forwarded on the wire).
- `TokenCache` keys by `storage_key()`.

### Selection precedence (explicit, phase A)

1. Per-call: HTTP `?account=<label>` / `ProxyRequest.account` / `access_token_for`.
2. Env: `SYNAPS_ACCOUNT_<PROVIDER>` with `-` → `_`, e.g. `SYNAPS_ACCOUNT_OPENAI_CODEX=astra2`.
3. Config: `auth.account.openai-codex = astra2` (`AuthConfig.accounts: BTreeMap<String, String>`; parsed in the existing `auth.*` branch; unknown providers preserved with a warning).
4. `default`.

Unknown label → `BrokerError::UnknownAccount { provider, label }` → HTTP 404 `{"error":"unknown account"}`. Error text names the label, never the token.

### HTTP surface (`src/cmd/auth_broker.rs`)

| Endpoint | Change |
|---|---|
| `GET /token?provider=X&account=Y` | `account` optional; `Y` must pass label grammar or 400. Log line includes label. |
| `POST /proxy` | body `account` optional; same validation. |
| `GET /usage?provider=X&account=Y` | phase B: typed usage op per provider (today Anthropic-only, no provider param). Token stays broker-side. |
| `GET /capabilities` | each OAuth provider entry gains `accounts: [ {label, identity, account_id_prefix, expires, configured, cooldown_until} ]`. No secret values (existing test `capabilities_never_leak_secrets` extended). |
| `POST /accounts/cooldown` | phase C: client reports `{provider, account, until_ms, reason}` after a provider limit response. Machine-auth. |

### CLI

| Command | Behaviour |
|---|---|
| `synaps login --provider openai-codex --account astra2` | Logs in and stores under `openai-codex@astra2`. Provider login fns (`openai_codex::login`, `xai::login`, `kimi_code::login`, `github_copilot::login`, `google_gemini::login`, anthropic) currently call `save_provider_auth(PROVIDER, …)` internally; they take a `&CredentialRef` (or storage key) so the write lands in the right slot in one step — no move-after-login. |
| dedupe on login | If the new credential's `accountId` equals another stored entry for the same provider, refuse and do **not** persist ("already connected as `<label>`"). Two slots sharing one seat is how a week's quota gets spent twice. Providers without an id: warn only. |
| `synaps auth list [--provider X] [--json]` | Table: provider · label · identity · expires · selected · cooldown. Local only (reads `auth.json`); remote clients use `/capabilities`. |
| `synaps auth remove --provider X --account Y` | Deletes the one key (lock + merge write). Refuses `default` unless `--yes`. |
| `synaps auth use --provider X --account Y\|auto` | Writes `auth.account.X = Y` to the profile config via the existing config read-modify-write. |
| `synaps status --provider X [--account Y \| --all] [--json]` | phase B. |

### Automatic selection & rotation (phase C — after usage ops land)

- `AccountSelector::Auto`: broker picks, in configured order, the first account with no active cooldown; if usage snapshots exist, prefer lowest weekly utilization. Fail closed: if no account has proven capacity, return `BrokerError::NoAccountAvailable { provider, earliest_reset }` rather than a token that will die mid-turn.
- Cooldown sources: provider 429/limit response (`reset_at` when present), the usage poll (`limit_reached: true`), or a client report. Cooldown state is in-memory on the broker (`0600` snapshot optional) and shown in `/capabilities`.
- Codex stream path: on `limit_reached` for account A with `Auto` selected, re-resolve once and retry with account B; surface `switched account astra1 → astra2` in the trace. One retry, never a loop.

### Quota keeper (phase D — separate plan)

First-use anchoring is documented for some Codex reset paths, but this is **not a universal documented guarantee for every natural weekly rollover** (see the safety constraints in the goal plan). The keeper polls provider-reported usage and, only after explicit per-account opt-in and fresh headroom evidence, can send one minimal, tool-free Codex request to reduce activation delay. It verifies a new window from a later provider-reported reset rather than assuming `now + 7 days`. It never runs an agent turn, purchases or redeems a reset. See [the implemented keeper runbook](../providers/quota-keeper.md) for its limits and live-validation status.

## Non-goals

- Changing the rotating-refresh-token invariant. One refresher per *credential* (was: per provider).
- Sharing one label namespace across providers. Labels are scoped to a provider.
- Auto-creating accounts or bypassing provider limits. This routes tools at the operator's own seats.

## Security notes

- Label validation happens at every boundary (CLI arg, env, config, HTTP query, proxy body) before it is used as a JSON key or appears in a log line.
- `/capabilities` and `auth list --json` expose `identity` (an email) — same trust level as the machine token; documented. `account_id` is shown as an 8-char prefix.
- Error bodies to remote clients stay generic (`unknown account`, `token refresh failed`), matching the existing "omit upstream body" tests.
- `auth remove` never leaves a partial file (same lock + tmp/rename path).

## Phasing & tests

| Phase | Scope | Tests |
|---|---|---|
| **A (this PR)** | `account.rs`; metadata fields; `ensure_fresh_credential`; broker trait `access_token_for`/`accounts`; policy from config/env; HTTP `account` param + capabilities listing; `login --account` + dedupe; `auth list/remove/use`; relax broker startup check | label grammar & key round-trip; mixed-key storage round-trip; gate isolation (two accounts refresh concurrently, one gate per key); `/token` with valid/unknown/invalid account; `/capabilities` lists accounts without secrets; login dedupe refuses same `accountId`; config parse of `auth.account.*`; env precedence |
| B | usage ops for codex/kimi/xai; `/usage?provider&account`; `synaps status --all --json` | per-provider usage parsers against recorded fixtures |
| C | `Auto` selector, cooldown, rotate-on-limit in Codex stream | policy unit tests; stream retry-once test |
| D | quota-keeper daemon | separate plan |

## Open questions for review

1. Key format `provider@label` (flat, zero-migration) vs. nested `accounts` object (cleaner, needs migration + typed-struct churn). Proposal: flat.
2. Should `auth.account.<provider> = auto` be the shipped default once phase C lands, or stay opt-in? Proposal: opt-in per provider.
3. Expose `identity` (email) in `/capabilities` to remote clients, or local-only? Proposal: expose (machine-token trust level), redact with the existing `redact_emails`-style flag if requested.
