# Grok (xAI) OAuth for `synaps login`

Status: **draft / scaffolding** — branch `feat/grok-xai-oauth`

Adds xAI (Grok) as an OAuth login provider in `synaps login`, mirroring the
existing `openai-codex` OAuth provider. Reference for the flow shape:
<https://pi.dev/packages/pi-xai-oauth> (the `pi` ecosystem's `pi-xai-oauth`
package — same PKCE + local-callback flow, different host CLI).

## Flow (from reference)

1. Start a local HTTP callback server on `127.0.0.1`.
2. Build an xAI OAuth authorize URL with a PKCE challenge.
3. Open the default browser to the xAI login page (manual-paste fallback in the TUI).
4. xAI redirects back to the local callback with `code` + `state`.
5. Exchange the authorization code for `access_token` + `refresh_token`.
6. Persist tokens; refresh automatically before expiry.
7. (Nice-to-have) auto-detect existing Grok CLI creds at `~/.grok/auth.json`.

API surface once authed: OpenAI Responses API format via `https://api.x.ai/v1`.
Models mentioned upstream: `grok-4.5` (default), `grok-4.3`, `grok-build`,
`grok-composer-2.5-fast`, etc.

## Integration points in this repo (parallels `openai-codex`)

- `crates/agent-core/src/core/auth/xai_grok.rs` — **new** provider module.
  Model it on `openai_codex.rs`: `login()` + `build_auth_url()` + token exchange,
  reusing `generate_code_verifier` / `generate_code_challenge` / `generate_state`
  / `start_callback_server` / `open_browser` / `save_provider_auth`.
- `crates/agent-core/src/core/auth/mod.rs`
  - `mod xai_grok;` (near line 19, with the other `mod` decls)
  - `pub use xai_grok::{login as login_xai_grok, ...};` (near line 29)
- `src/cmd/login.rs` — add a `LoginProvider { key: "xai-grok", ... }` entry in
  `login_providers()` (near line 283) and route it in the login match.
- `crates/agent-core/src/core/auth/credential_source.rs` — teach credential
  resolution / refresh about the `xai-grok` provider.
- `crates/agent-engine/src/runtime/openai/registry.rs` — register the Grok
  models against the xAI base URL (`https://api.x.ai/v1`).

## Constants to fill in (need confirmation from xAI OAuth docs)

```
PROVIDER      = "xai-grok"
CLIENT_ID     = "<xai oauth client id>"      // TODO confirm
AUTHORIZE_URL = "https://<xai-oauth-host>/oauth/authorize"   // TODO confirm
TOKEN_URL     = "https://<xai-oauth-host>/oauth/token"       // TODO confirm
CALLBACK_PORT = <pick unused port, e.g. 1456>
REDIRECT_URI  = "http://localhost:<port>/auth/callback"
SCOPE         = "<xai scopes>"               // TODO confirm
```

> ⚠️ The reference README does not publish the exact client_id / authorize /
> token endpoints or scopes. These must be confirmed from xAI's official OAuth
> docs (or by inspecting the Grok CLI) before implementation — do not guess and
> ship. Everything else follows the `openai-codex` template 1:1.

## Test parallels

- Mirror `openai_codex::tests` and the `mod.rs` regression suite
  (`manual_paste_to_callback`, state validation) for `xai_grok`.
