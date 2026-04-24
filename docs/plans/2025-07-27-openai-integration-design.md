# OpenAI (Codex) Integration — Design Document

**Date:** 2025-07-27  
**Branch:** `feat/openai-integration`  
**Goal:** Add native OpenAI support with OAuth "Sign in with ChatGPT" login, so users can chat with OpenAI models (GPT-4.1, o3, Codex, etc.) using their ChatGPT subscription — no API key required.

---

## 1. Context & Research Summary

### Current Architecture
Synaps CLI has a two-path routing system:
- **Anthropic (native)** — OAuth or API key → `api.anthropic.com/v1/messages` with Anthropic SSE format
- **OpenAI-compat (generic)** — API key only → `{base_url}/chat/completions` with OpenAI SSE format, translated back to Anthropic-shaped events

The existing OpenAI-compat path (`src/runtime/openai/`) already supports 18+ providers (Groq, Cerebras, etc.) via `/chat/completions` and API keys. What's missing is **native OpenAI as a first-class provider with OAuth**.

### OpenAI OAuth Flow (Confirmed from Codex CLI Source)
OpenAI has a full OIDC/OAuth 2.0 server powered by Auth0:

| Field | Value |
|---|---|
| **Authorization endpoint** | `https://auth.openai.com/oauth/authorize` |
| **Token endpoint** | `https://auth.openai.com/oauth/token` |
| **Client ID** | `app_EMoamEEZ73f0CkXaXp7hrann` (Codex CLI's public client ID) |
| **Callback port** | `1455` (Codex CLI default) |
| **Callback path** | `/auth/callback` |
| **Scopes** | `openid profile email offline_access` |
| **PKCE** | Required (S256) |
| **Extra params** | `id_token_add_organizations=true`, `codex_cli_simplified_flow=true` |

The access token from this flow is a Bearer token used against `https://api.openai.com/v1/responses` (the new Responses API) or `/v1/chat/completions` (legacy).

### API: Responses API vs Chat Completions
Codex CLI uses the **Responses API** (`POST /v1/responses`), which is OpenAI's newer endpoint. However, `/v1/chat/completions` still works and is simpler. Since our codebase already has a full Chat Completions implementation, **we'll use Chat Completions** for the OpenAI-with-OAuth path. This avoids building an entirely new message format translator. The existing `openai/stream.rs`, `openai/translate.rs`, and `openai/wire.rs` work perfectly.

### Key Insight: OpenAI-with-OAuth is just OpenAI-compat with a Bearer token
The only difference from our existing OpenAI-compat path is:
1. The API key comes from OAuth instead of an env var
2. The base URL is `https://api.openai.com/v1` 
3. The token needs auto-refresh

---

## 2. Design

### Approach: Extend Existing Infra (Recommended)

Rather than building a parallel native path (like Anthropic), we extend the existing OpenAI-compat provider system to support OpenAI OAuth as a special case.

#### A. Auth Module Extension (`src/core/auth/`)

Make the auth system multi-provider:

```rust
// auth.json becomes:
{
  "anthropic": { "type": "oauth", "refresh": "...", "access": "...", "expires": ... },
  "openai": { "type": "oauth", "refresh": "...", "access": "...", "expires": ... }
}
```

- `AuthFile` gains an `openai: Option<OAuthCredentials>` field
- New constants for OpenAI OAuth (authorize URL, token URL, client ID, scopes, callback port)
- `login()` accepts a provider parameter: `synaps login` (Anthropic, default) vs `synaps login --openai`
- Separate PKCE, callback server, and token exchange for OpenAI (different endpoints, different client ID, different port)

#### B. Provider Routing Enhancement (`src/runtime/openai/`)

Add `openai` as a special provider key in the registry:

```rust
// In registry.rs — special "openai" provider
ProviderSpec {
    key: "openai",
    name: "OpenAI",
    base_url: "https://api.openai.com/v1",
    env_vars: &["OPENAI_API_KEY"],
    default_model: "gpt-4.1",
    models: &[
        ("gpt-4.1", "GPT-4.1", "A+"),
        ("gpt-4.1-mini", "GPT-4.1 Mini", "B+"),
        ("gpt-4.1-nano", "GPT-4.1 Nano", "B"),
        ("o3", "o3", "S+"),
        ("o4-mini", "o4-mini", "A+"),
    ],
}
```

**Key change in `resolve_api_key()`**: For the `openai` provider, also check `auth.json` for OAuth credentials before falling back to env vars.

#### C. Token Refresh in Runtime (`src/runtime/auth.rs`)

Extend `AuthMethods` to also check/refresh OpenAI tokens when an OpenAI model is being used. The existing `refresh_if_needed()` pattern works — just need an OpenAI variant that hits `https://auth.openai.com/oauth/token` with a `refresh_token` grant.

#### D. Login Command (`src/cmd/login.rs`)

Add `--openai` flag:
```
synaps login          # Anthropic (default)  
synaps login --openai # OpenAI (Sign in with ChatGPT)
```

#### E. Model Usage

Users select OpenAI models with the existing provider shorthand:
```
/model openai/gpt-4.1
/model openai/o3
```

---

## 3. File Changes Summary

| File | Action | Description |
|---|---|---|
| `src/core/auth/mod.rs` | Modify | Add OpenAI constants, make `login()` provider-aware |
| `src/core/auth/mod.rs` | Modify | New `AuthFile` shape with optional `openai` field |
| `src/core/auth/token.rs` | Modify | Add `exchange_code_for_tokens_openai()`, `refresh_token_openai()`, `ensure_fresh_openai_token()` |
| `src/core/auth/callback.rs` | Modify | Support configurable callback port (53692 for Anthropic, 1455 for OpenAI) |
| `src/core/auth/pkce.rs` | Modify | Add `build_openai_auth_url()` |
| `src/core/auth/storage.rs` | Modify | Update `save_auth()` and `load_auth()` for multi-provider |
| `src/runtime/openai/registry.rs` | Modify | Add `openai` provider spec with models |
| `src/runtime/openai/registry.rs` | Modify | `resolve_api_key()` checks OAuth credentials for `openai` |
| `src/runtime/auth.rs` | Modify | Add OpenAI token refresh path |
| `src/runtime/api.rs` | Modify | Pass OpenAI OAuth token when routing to OpenAI |
| `src/cmd/login.rs` | Modify | Add `--openai` flag |
| `src/main.rs` | Modify | Add `--openai` flag to login subcommand |

---

## 4. Risk Assessment

| Risk | Mitigation |
|---|---|
| OpenAI blocks non-Codex client IDs | Use the same `app_EMoamEEZ73f0CkXaXp7hrann` client ID (public, used by multiple 3rd-party tools like term-llm, Roo Code) |
| OpenAI changes OAuth endpoints | All endpoints are behind constants, easy to update |
| Token format incompatible with Chat Completions | Confirmed working — the OAuth token is a standard Bearer token accepted by all OpenAI API endpoints |
| Breaking existing auth.json | Backward-compatible — new `openai` field is `Option<>`, old format still works |

---

## 5. Non-Goals (YAGNI)

- ❌ Responses API support (Chat Completions works fine)
- ❌ OpenAI-specific tool schemas (our translator already handles this)
- ❌ Keyring/credential store (file-based like Anthropic is fine for now)
- ❌ Device code flow (PKCE + localhost callback matches our existing pattern)
- ❌ Organization/workspace selection (can add later if needed)
