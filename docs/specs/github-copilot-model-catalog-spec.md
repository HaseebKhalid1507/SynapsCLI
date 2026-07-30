# GitHub Copilot model catalog (C2 catalog only)

Status: **implementation target for C2 catalog slice** — branch `feat/github-copilot-oauth`
Worktree: `/home/jr/Projects/Maha-Media/.worktrees/SynapsCLI-github-copilot-oauth`
Scope: **model discovery / catalog registration only**. No chat/completions routing,
no runtime inference headers beyond the catalog GET, no policy enablement posts.

---

## ASSUMPTIONS

1. C1 OAuth (device flow + session mint + broker session vending) is already green.
2. Account-specific live discovery is preferred over a hard-coded full catalog because
   plan, org policy, and preview gates change which IDs appear.
3. The community-observed `GET https://api.githubcopilot.com/models` surface is
   **experimental** — not a documented stable third-party public product API.
4. The GitHub user token never leaves the broker; catalog fetch uses only the
   short-lived Copilot session credential.
5. Inference / chat routing is a later slice; this slice may allowlist `/models`
   only on the broker proxy for `github-copilot`.

---

## Evidence tiers

| Tier | Meaning |
| --- | --- |
| **V** | Official GitHub documentation (supported models, comparison, retirement, plan tables). |
| **C** | Community / multi-client observation + live authenticated discovery against the operator account. Experimental. |
| **U** | Unknown / account-specific / needs re-verification. |

---

## Official sources (V)

| Source | URL |
| --- | --- |
| Supported AI models | https://docs.github.com/en/copilot/reference/ai-models/supported-models |
| AI model comparison | https://docs.github.com/en/copilot/reference/ai-models/model-comparison |
| Plan availability table (docs source) | https://github.com/github/docs/blob/main/data/tables/copilot/model-supported-plans.yml |
| Release status table | https://github.com/github/docs/blob/main/data/tables/copilot/model-release-status.yml |
| Retirement history | https://github.com/github/docs/blob/main/data/tables/copilot/model-deprecation-history.yml |
| Auto model selection table | https://github.com/github/docs/blob/main/data/tables/copilot/auto-model-selection.yml |

### Official high-value display names currently listed (V)

OpenAI: GPT-5.3-Codex, GPT-5.4, GPT-5.4 mini, GPT-5.5, GPT-5.6 Luna/Sol/Terra, GPT-5 mini
Anthropic: Claude Sonnet 4.6 / 5, Claude Opus 4.7 / 4.8 (+ fast mode preview), Claude Fable 5, Claude Haiku 4.5
Google: Gemini 3.1 Pro, Gemini 3.5 Flash, Gemini 3 Flash, Gemini 2.5 Pro

### Official retirement / exclude list (V)

Do **not** seed as curated fallback (retired or scheduled): GPT-4.1 (2026-06-01),
GPT-5.2 / GPT-5.2-Codex (2026-06-01), Gemini 3 Pro (2026-03-26), Grok Code Fast 1
(2026-05-15), Claude Sonnet 4 (2026-05-01), older GPT-5.1 / o-series / Claude 3.x, etc.

**Auto** is a product selection mode in official docs, **not** observed as a
`/models` wire id on the live personal endpoint (U/C).

---

## Experimental discovery protocol (C)

### Endpoint (pinned for v1 catalog)

```http
GET https://api.githubcopilot.com/models
```

Also observed in community clients: base may be rewritten from session mint
`endpoints.api` (e.g. `api.individual.githubcopilot.com`). **v1 pins**
`https://api.githubcopilot.com` only (fail closed on other hosts). Enterprise /
business hosts are out of scope for this slice.

### Auth / headers (C — experimental)

```http
Authorization: Bearer <copilot_session_token>
User-Agent: SynapsCLI/0.6.0
Editor-Version: vscode/1.107.0
Editor-Plugin-Version: copilot-chat/0.35.0
Copilot-Integration-Id: vscode-chat
X-Github-Api-Version: 2025-10-01
Accept: application/json
```

Notes:

- Session token is the short-lived Copilot credential from C1 mint (`tid=…`),
  **not** the GitHub user token.
- Integration/editor headers match the C1 mint surface. Self-identifying
  `User-Agent` remains honest; integration id is community-required (ToS risk
  already documented in the OAuth spec).
- `X-Github-Api-Version` values observed in clients: `2025-10-01` (live OK),
  `2026-06-01` (community). Pin the live-verified value for v1.
- Do **not** send this header vocabulary to `api.github.com` REST.

### Response shape (C — live, redacted)

```json
{
  "object": "list",
  "data": [
    {
      "id": "claude-sonnet-4.6",
      "name": "Claude Sonnet 4.6",
      "vendor": "Anthropic",
      "object": "model",
      "preview": false,
      "model_picker_enabled": true,
      "model_picker_category": "versatile",
      "is_chat_default": false,
      "is_chat_fallback": false,
      "policy": { "state": "enabled", "terms": "…" },
      "capabilities": {
        "type": "chat",
        "family": "claude-sonnet-4.6",
        "limits": {
          "max_prompt_tokens": 200000,
          "max_output_tokens": 64000,
          "max_context_window_tokens": 264000
        },
        "supports": { "vision": true, "tool_calls": true, "streaming": true }
      },
      "supported_endpoints": ["/chat/completions", "/v1/messages"]
    }
  ]
}
```

### Live wire IDs observed on the signed-in personal account (C)

High-value chat IDs established by live discovery (2026-07 operator account):

| Wire id | Display name | Notes |
| --- | --- | --- |
| `gpt-5.3-codex` | GPT-5.3-Codex | picker enabled; chat fallback |
| `gpt-5.4` | GPT-5.4 | picker enabled |
| `gpt-5.4-mini` | GPT-5.4 mini | picker enabled |
| `gpt-5.5` | GPT-5.5 | present; plan/policy may disable picker |
| `gpt-5.6-luna` | GPT-5.6 Luna | picker enabled |
| `gpt-5.6-terra` | GPT-5.6 Terra | picker enabled |
| `gpt-5-mini` | GPT-5 mini | picker enabled |
| `claude-sonnet-4.6` | Claude Sonnet 4.6 | picker enabled |
| `claude-sonnet-5` | Claude Sonnet 5 | picker enabled |
| `claude-opus-4.7` | Claude Opus 4.7 | present; may be policy-gated |
| `claude-opus-4.8` | Claude Opus 4.8 | present; may be policy-gated |
| `claude-opus-4.8-fast` | Claude Opus 4.8 (fast mode) | present; may be policy-gated |
| `claude-fable-5` | Claude Fable 5 | present; may be policy-gated |
| `claude-haiku-4.5` | Claude Haiku 4.5 | picker enabled |
| `gemini-3.1-pro-preview` | Gemini 3.1 Pro | preview wire id |
| `gemini-3.5-flash` | Gemini 3.5 Flash | picker enabled |
| `gemini-3-flash-preview` | Gemini 3 Flash (Preview) | preview wire id |

**Not returned** for this account (do not invent): `gpt-5.6-sol`, bare `auto`,
`claude-sonnet-4` (retired), `gemini-3-pro` (retired).

Also returned but **not** curated fallback: embeddings (`text-embedding-*`),
completion (`gpt-41-copilot`), utility (`trajectory-compaction`), legacy GPT-4o /
3.5 family.

---

## Product behavior

### Live discovery (preferred)

1. When `github-copilot` is logged in, catalog fetch goes through the credential
   broker proxy: `GET /models` only.
2. Broker resolves `OAuthProviderId::GitHubCopilot` session token, attaches
   catalog headers, pins base URL, disables redirects, bounds timeouts/body.
3. Parser keeps `capabilities.type == "chat"` models with non-empty `id`.
4. Runtime id is `github-copilot/<wire-id>`.
5. Never log or return the session token or GitHub user token.

### Curated static fallback

Used when live discovery is unavailable (offline / not configured / transport
error paths that intentionally fall back — UI static seeds). Contains **only**
wire IDs established by live discovery/fixtures, not guessed display-name slugs.

Fallback set (ordered):

1. `gpt-5.3-codex` — GPT-5.3-Codex
2. `gpt-5.4` — GPT-5.4
3. `gpt-5.5` — GPT-5.5
4. `gpt-5.6-luna` — GPT-5.6 Luna
5. `gpt-5.6-terra` — GPT-5.6 Terra
6. `claude-sonnet-4.6` — Claude Sonnet 4.6
7. `claude-sonnet-5` — Claude Sonnet 5
8. `claude-opus-4.7` — Claude Opus 4.7
9. `claude-opus-4.8` — Claude Opus 4.8
10. `claude-fable-5` — Claude Fable 5
11. `gemini-3.1-pro-preview` — Gemini 3.1 Pro
12. `gemini-3.5-flash` — Gemini 3.5 Flash

Excluded from fallback despite official docs: `gpt-5.6-sol` (not on this
account’s live list), Auto (not a wire id).

### Fail closed

- Malformed JSON / missing `data` array → error (no partial silent invent).
- Empty model id → skip entry.
- Non-chat capability types → skip (embeddings/completion/utility).
- Redirect status from production client → error.
- Oversized body → error (catalog body cap).
- Unknown / non-pinned base host → not used.

### Non-goals (this slice)

- `POST /chat/completions` or `/responses` routing
- Model policy enable (`POST /models/{id}/policy`)
- Enterprise/business host derivation
- GitHub Models marketplace catalog (`models.github.ai`) — different product

---

## Testing strategy

- Unit: parse fixture JSON; static descriptor ids; filter non-chat.
- Broker: `github-copilot` allowlisted for `/models` only; other OAuth providers
  still denied; `/chat/completions` still denied for Copilot in this slice.
- Zero-network e2e harness for parser + static fallback registration.
- TUI: logged-in `github-copilot` section shows static fallback immediately.
- RED before GREEN for each behavior.

---

## Boundaries

**Always**

- Prefer broker-proxied live discovery when credentials exist.
- Never vend/log GitHub user token or session token.
- Pin host/path/headers; bound network; fail closed.
- Seed only fixture/live-established wire IDs.

**Ask first**

- Expanding broker allowlist to chat/inference paths.
- Enterprise host support.
- Policy-enable automation.

**Never**

- Guess wire ids from display names.
- Ship retired models as curated defaults.
- Use GitHub Models catalog as a substitute for Copilot discovery.
