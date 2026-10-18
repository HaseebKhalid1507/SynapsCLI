# GPT-6 Astra on Codex OAuth

Select `openai-codex/gpt-6-astra` with `/model`, then `/effort ultra`.
The logical setting remains `ultra` in configuration/session state. It is
not a numeric thinking budget and is not an alias for `/effort xhigh`.

## Request contract

| Selection | Foreground wire effort | Delegation mode | Same-model worker |
| --- | --- | --- | --- |
| Astra `ultra` | `xhigh` | Proactive, subject to host authorization | Inherits logical Ultra, sends `xhigh`, no recursive mode |
| Astra `max` | `max` | Explicit request only | Model default |
| Astra `xhigh` | `xhigh` | Explicit request only | Model default |
| Sol/Terra `ultra` without a live override | `max` | Proactive, subject to host authorization | Inherits logical Ultra, sends `max`, no recursive mode |

The live Codex catalog advertises Astra's `multi_agent_reasoning_effort: xhigh`.
This is an **Ultra request override**, not a worker-only default. The official
Codex implementation applies it to both CLI and spawned sessions:

- [client.rs: reasoning_effort_for_request](https://github.com/openai/codex/blob/574a36ff99f0807a24f5b043f593122bf151908d/codex-rs/core/src/client.rs#L186)
- [client_tests.rs: foreground and spawned-session assertions](https://github.com/openai/codex/blob/574a36ff99f0807a24f5b043f593122bf151908d/codex-rs/core/src/client_tests.rs#L407)

Synaps accepts an override only if it is a recognized wire effort in that
exact model's supported ladder. Missing, unknown, logical (e.g. `ultra`),
or unsupported overrides fall back to supported `max`, then the last
supported wire effort. Unlike upstream's final unconditional `medium`
fallback, a malformed ladder containing no usable wire effort is rejected
before credentials/network. Ultra still requires exact V2 capability.

Live rows remain authoritative; an absent live override does not borrow
Astra's static `xhigh`. Cache refresh replaces the field with the rest of the
row. The offline Astra seed includes the observed `xhigh` override.

## Worker policy

`subagent`, `subagent_start`, and `subagent_resume` receive a host-created
foreground plan through the tool context. After authorization and exact model
selection, workers using the same provider-qualified identity inherit logical
Ultra. Each worker request revalidates current capabilities and computes its
wire effort. Worker role and the restricted registry continue to prevent
recursive delegation. Explicitly selecting another model/provider keeps that
model's defaults; no cross-provider effort or authorization is inferred.
Resume uses the invoking foreground's current mode; switching out of Ultra
therefore restores the normal worker defaults on subsequent starts/resumes.

## Hosted web search (`SYNAPS_CODEX_WEB_SEARCH`)

Opt-in. `live` / `on` / `true` / `1` / `yes` attaches OpenAI's hosted
`web_search` tool to Codex requests; `cached` attaches it with
`external_web_access: false`; anything else leaves it off.

- **Where it is read.** The variable is read from the environment of the
  process that runs the turn. In daemon mode that is the **daemon's**
  environment (inherited from the shell that first spawned it), not the
  attaching client's. Setting or unsetting it in another shell does nothing
  until `synaps daemon stop`. The first Codex request of each session prints a
  one-line `🔎 Codex web_search is on` notice when it is attached.
- **Which requests.** Foreground turns only. Subagent workers also need
  `SYNAPS_CODEX_WEB_SEARCH_WORKERS=1`; internal turns never get it. Models
  that can't use it are skipped: `gpt-5.3-codex-spark` (text-only), and any
  live `/codex/models` row with `supports_search_tool: false`.
- **Visibility.** Each completed `web_search_call` item in the Responses
  stream is shown as a `🔎 Codex web_search (<action>, <status>): <query>`
  notice and logged at info (`target: synaps::web_search`). The search runs
  on OpenAI's servers, so `before_tool_call` / `after_tool_call` hooks never
  see it and can't block it.

## Codex HTTP 4xx errors

A terminal 4xx from `/codex/responses` reads a bounded slice of the body
(8 KiB). The error shows the sanitized `error.type` / `error.code`
identifiers, the `param` path, and the provider message after redaction:
one line, capped at 200 characters, quoted spans over 48 characters and
token-shaped runs replaced with `[redacted]`, and the whole message dropped
if it contains JSON or HTML structure. The trace terminal code carries only
the identifiers (`http_400:invalid_request_error:unsupported_value`). "Not
supported when using Codex with a ChatGPT account" adds a hint to switch
account or model.

## Discovery and picker

Discovery uses `/backend-api/codex/models?client_version=0.153.3`, not the
ChatGPT web picker at `/backend-api/models`. The version is a Codex protocol
version, independent of Synaps' package version. Astra requires >= 0.153.0.

The TUI's Codex picker remains source-controlled (eight static OAuth entries,
Astra first). Live discovery supplies runtime capability metadata; it does
not turn the TUI into a live arbitrary-slug picker. This distinction matters
when diagnosing an offline seed versus a live catalog result.

No priority service tier, larger context opt-in, or verbosity change is
introduced by the Ultra fix. Existing prompt-cache keys and retry bytes stay
stable. `/effort max` continues to send `max`.

## Verification

Local regression coverage includes live-row parsing, static/cache metadata,
invalid/absent overrides, unchanged explicit levels, V2 authorization,
worker initialization in all three paths, TUI picker rows, and loopback HTTP
capture of foreground/worker requests plus byte-identical retries.

An opt-in live smoke uses broker-owned credentials, sends two tiny requests
without executing tools, and prints only the role/mode/result:

```bash
SYNAPS_ASTRA_LIVE_SMOKE=1 cargo test -p synaps-engine --lib \
  astra_ultra_live_smoke -- --ignored --nocapture
```

This requires an authenticated account with Astra access and consumes account
usage. The normal suite never runs this test.
