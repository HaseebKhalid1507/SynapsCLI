# Google Gemini CLI OAuth and Code Assist Runtime Specification

Status: approved for implementation

## Objective
Add `google-gemini` as a typed OAuth provider to `synaps login`, backed by Google’s installed-app OAuth flow and the Gemini CLI Code Assist service. Provide broker-owned refresh credentials, model discovery/catalog integration, and broker-proxied streaming runtime inference without exposing credentials.

## Assumptions
1. “Google in the same vein” means Google account OAuth for Gemini CLI / Gemini Code Assist, not merely another Gemini API-key alias.
2. Synaps may reference the Apache-2.0 `google-gemini/gemini-cli` implementation and public Google OAuth documentation, but will implement the protocol independently in Rust.
3. Synaps requires its own Google Desktop OAuth registration via `SYNAPS_GOOGLE_GEMINI_CLIENT_ID` (and optional public-client value via `SYNAPS_GOOGLE_GEMINI_CLIENT_SECRET`); it does not embed or borrow another product's registration. The `cloudcode-pa.googleapis.com/v1internal` protocol remains a product-client/community-observed integration surface and is marked experimental unless Google documents it as a stable third-party API.
4. Both free managed-project onboarding and existing Standard/Enterprise project accounts must fail safely; unattended tests use fixtures and never perform real onboarding.

## Commands
- `CARGO_BUILD_JOBS=8 cargo test -p synaps-core google_gemini -- --test-threads=1`
- `CARGO_BUILD_JOBS=8 cargo test -p synaps-engine google_gemini -- --test-threads=1`
- `CARGO_BUILD_JOBS=8 cargo test --test google_gemini_oauth_e2e -- --test-threads=1`
- `CARGO_BUILD_JOBS=8 cargo test --test google_gemini_runtime_e2e -- --test-threads=1`
- `CARGO_BUILD_JOBS=8 cargo check -p synaps-core -p synaps-engine -p synaps-tui`

## Structure
- Auth/provider/broker: `crates/agent-core/src/core/auth/`
- Catalog/runtime wire: `crates/agent-engine/src/runtime/`
- TUI model registration: `crates/agent-tui/src/tui/models/`
- Unattended harnesses: `tests/google_gemini_*_e2e.rs`

## Protocol
- Canonical provider: `google-gemini`; parsing aliases are migration-only.
- Authorization: `https://accounts.google.com/o/oauth2/v2/auth`, authorization code + PKCE, loopback callback, exact state validation.
- Token endpoint: `https://oauth2.googleapis.com/token`; scopes: cloud-platform, userinfo.email, userinfo.profile; offline access and consent semantics required for refresh issuance.
- Production login fails closed until `SYNAPS_GOOGLE_GEMINI_CLIENT_ID` names a Synaps-owned Google Desktop OAuth registration. `SYNAPS_GOOGLE_GEMINI_CLIENT_SECRET` is accepted only as an optional installed-app public-client value and is never logged.
- Broker stores refresh/access tokens atomically. Remote `/token` may return access token + expiry only, never refresh token or client credentials.
- Code Assist host pinned to `https://cloudcode-pa.googleapis.com`; only reviewed `v1internal` methods are allowed.
- Setup resolves project/tier through bounded `loadCodeAssist`/onboarding operations. Validation links are displayed but never automatically followed by the broker.
- Streaming uses `v1internal:streamGenerateContent` and Gemini content/tool-call translation. Redirects and arbitrary destinations are denied.

## Catalog
Prefer account-specific service metadata when a supported model-discovery operation is verified. Otherwise expose a conservative current text/tool-capable fallback whose wire IDs are established by official Gemini CLI source or fixtures. Do not guess IDs or expose embedding/media-only models.

## Testing
Strict red-before-green. Unit tests cover URL/state/token parsing, refresh, path allowlists, body limits, project metadata, Gemini request translation, SSE decoding, text/tool calls, cancellation, and secret-safe errors. E2E harnesses use localhost fake OAuth and Code Assist servers through explicit test seams and perform zero external network calls.

## Boundaries
### Always
- Broker owns refresh tokens and signs upstream requests.
- Pin schemes, hosts, and method paths; reject redirects.
- Bound connect/request timeouts, buffered bodies, and onboarding polling.
- Preserve unknown auth metadata and write credentials atomically.

### Ask first
- New dependencies, persistent schema changes outside existing auth metadata, or CI changes.

### Never
- Log tokens, vend refresh tokens, read `auth.json` in runtime/TUI, accept caller URLs, embed a true confidential secret, silently fall back to API-key environment variables, or claim the Code Assist internal API is stable/public.

## Success criteria
A user can run `synaps login --provider google-gemini`, receive an explicit registration prerequisite until a Synaps-owned Google Desktop OAuth client is configured, then complete Google authorization, see account-available Gemini models, and stream text/tool calls through a broker-only route. All zero-network harnesses pass and malformed/redirected/oversized/unknown requests fail closed.
