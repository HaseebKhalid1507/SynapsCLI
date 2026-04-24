# OpenAI Integration — Implementation Plan

**Goal:** Add native OpenAI OAuth ("Sign in with ChatGPT") + OpenAI model routing so users can use GPT-4.1, o3, etc. with their ChatGPT subscription.  
**Architecture:** Extend existing OAuth auth module to support multiple providers; add `openai` as a first-class provider in the OpenAI-compat registry; reuse existing Chat Completions streaming path.  
**Design Doc:** `docs/plans/2025-07-27-openai-integration-design.md`  
**Estimated Tasks:** 10 tasks  
**Complexity:** Medium

---

### Task 1: Add OpenAI to AuthFile (backward-compatible)

**Files:**
- Modify: `src/core/auth/mod.rs`
- Modify: `src/core/auth/storage.rs`

**Step 1: Update AuthFile struct**

In `src/core/auth/mod.rs`, add OpenAI OAuth constants and update `AuthFile`:

```rust
// ── OpenAI Constants ──────────────────────────────────────────────────────
pub(super) const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub(super) const OPENAI_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub(super) const OPENAI_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub(super) const OPENAI_CALLBACK_PORT: u16 = 1455;
pub(super) const OPENAI_SCOPES: &str = "openid profile email offline_access";
```

Update `AuthFile`:
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthFile {
    pub anthropic: OAuthCredentials,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub openai: Option<OAuthCredentials>,
}
```

**Step 2: Update storage.rs**

In `save_auth`, accept a provider parameter. Add `save_openai_auth()` and `load_openai_auth()` helpers. Keep backward compat — if `openai` field is missing in JSON, it deserializes as `None`.

**Step 3: Verify**
```bash
cargo build 2>&1 | head -20
```

**Step 4: Commit**
```bash
git add -A && git commit -m "feat(auth): add OpenAI OAuth constants and multi-provider AuthFile"
```

---

### Task 2: Add OpenAI PKCE URL builder

**Files:**
- Modify: `src/core/auth/pkce.rs`

**Step 1: Add build_openai_auth_url()**

```rust
pub fn build_openai_auth_url(challenge: &str, state: &str, port: u16) -> String {
    let redirect_uri = format!("http://localhost:{}/auth/callback", port);
    format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}&id_token_add_organizations=true&codex_cli_simplified_flow=true",
        super::OPENAI_AUTHORIZE_URL,
        super::OPENAI_CLIENT_ID,
        urlencoding::encode(&redirect_uri),
        urlencoding::encode(super::OPENAI_SCOPES),
        challenge,
        state,
    )
}
```

**Step 2: Verify**
```bash
cargo build 2>&1 | head -20
```

**Step 3: Commit**
```bash
git add -A && git commit -m "feat(auth): add OpenAI PKCE auth URL builder"
```

---

### Task 3: Make callback server port-configurable

**Files:**
- Modify: `src/core/auth/callback.rs`

The callback server currently hardcodes the Anthropic callback path. Make `start_callback_server` accept a configurable callback path so both `/callback` (Anthropic) and `/auth/callback` (OpenAI) work. The port is already a parameter.

Update the Axum route to accept both paths, or parameterize it. OpenAI's redirect uses `/auth/callback`, Anthropic uses `/callback`.

**Step 1: Update start_callback_server to accept callback_path parameter**

**Step 2: Verify**
```bash
cargo build 2>&1 | head -20
```

**Step 3: Commit**
```bash
git add -A && git commit -m "feat(auth): make callback server path configurable for multi-provider"
```

---

### Task 4: Add OpenAI token exchange & refresh

**Files:**
- Modify: `src/core/auth/token.rs`

**Step 1: Add exchange_code_for_tokens_openai()**

Same as `exchange_code_for_tokens` but uses `OPENAI_CLIENT_ID`, `OPENAI_TOKEN_URL`, and `/auth/callback` redirect path.

**Step 2: Add refresh_openai_token()**

Same as `refresh_token` but uses OpenAI constants.

**Step 3: Add ensure_fresh_openai_token()**

Same as `ensure_fresh_token` but reads/writes the `openai` field of AuthFile.

**Step 4: Verify**
```bash
cargo build 2>&1 | head -20
```

**Step 5: Commit**
```bash
git add -A && git commit -m "feat(auth): add OpenAI token exchange and refresh"
```

---

### Task 5: Add OpenAI login flow

**Files:**
- Modify: `src/core/auth/mod.rs`

**Step 1: Add login_openai() function**

Mirror the existing `login()` function but use OpenAI constants:
- Different client ID, authorize URL, token URL
- Port 1455 instead of 53692
- Callback path `/auth/callback` instead of `/callback`
- Different scopes
- Save to `auth.json` under the `openai` key

**Step 2: Re-export from mod.rs**

Add `pub use` for the new `build_openai_auth_url`.

**Step 3: Verify**
```bash
cargo build 2>&1 | head -20
```

**Step 4: Commit**
```bash
git add -A && git commit -m "feat(auth): add OpenAI login flow (Sign in with ChatGPT)"
```

---

### Task 6: Add OpenAI provider to registry

**Files:**
- Modify: `src/runtime/openai/registry.rs`

**Step 1: Add OpenAI ProviderSpec**

Add to the `PROVIDERS` list:
```rust
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
        ("codex-mini", "Codex Mini", "A"),
    ],
},
```

**Step 2: Modify resolve_api_key() to check OpenAI OAuth**

For the `openai` provider specifically, also check `auth.json` for OAuth credentials:
```rust
fn resolve_api_key(provider_key: &str, env_vars: &[&str], overrides: &BTreeMap<String, String>) -> Option<String> {
    // Existing: config override
    if let Some(v) = overrides.get(provider_key) { ... }
    
    // Existing: env vars
    if let Some(v) = env_vars.iter().find_map(...) { return Some(v); }
    
    // NEW: For "openai" provider, check OAuth credentials
    if provider_key == "openai" {
        if let Ok(Some(auth)) = crate::auth::load_auth() {
            if let Some(ref openai_creds) = auth.openai {
                if openai_creds.auth_type == "oauth" && !openai_creds.access.is_empty() {
                    return Some(openai_creds.access.clone());
                }
            }
        }
    }
    
    None
}
```

**Step 3: Verify**
```bash
cargo build 2>&1 | head -20
```

**Step 4: Commit**
```bash
git add -A && git commit -m "feat(provider): add OpenAI as first-class provider with OAuth support"
```

---

### Task 7: Add OpenAI token refresh to runtime

**Files:**
- Modify: `src/runtime/auth.rs`
- Modify: `src/runtime/openai/registry.rs`

**Step 1: Add refresh path for OpenAI tokens**

When routing to the `openai` provider, check if the OAuth token needs refreshing before making API calls. Add a function that checks if the current model is an OpenAI model and refreshes if needed.

The simplest approach: in `resolve_api_key` for `openai`, call `ensure_fresh_openai_token()` synchronously (it's already blocking-friendly). Or better: add a pre-flight refresh check in `try_route()`.

**Step 2: Verify**
```bash
cargo build 2>&1 | head -20
```

**Step 3: Commit**
```bash
git add -A && git commit -m "feat(runtime): add OpenAI OAuth token refresh on API calls"
```

---

### Task 8: Update login command for OpenAI

**Files:**
- Modify: `src/cmd/login.rs`
- Modify: `src/main.rs`

**Step 1: Add --openai flag to CLI**

In `main.rs`, add `--openai` flag to the `login` subcommand:
```rust
Login {
    #[arg(long)]
    profile: Option<String>,
    #[arg(long, help = "Sign in with ChatGPT (OpenAI) instead of Anthropic")]
    openai: bool,
},
```

**Step 2: Update login.rs**

```rust
pub async fn run(profile: Option<String>, openai: bool) {
    if openai {
        // OpenAI login flow
        eprintln!("║  Sign in with your ChatGPT account  ║");
        match auth::login_openai().await { ... }
    } else {
        // Existing Anthropic flow
    }
}
```

**Step 3: Verify**
```bash
cargo build 2>&1 | head -20
```

**Step 4: Commit**
```bash
git add -A && git commit -m "feat(cli): add 'synaps login --openai' command"
```

---

### Task 9: Wire everything together & test build

**Files:**
- Modify: `src/lib.rs` (ensure new exports if needed)

**Step 1: Full build verification**
```bash
cargo build 2>&1
```

**Step 2: Run existing tests**
```bash
cargo test 2>&1
```

**Step 3: Fix any compilation issues**

**Step 4: Commit**
```bash
git add -A && git commit -m "fix: resolve compilation issues for OpenAI integration"
```

---

### Task 10: Manual integration test

**Step 1: Test login flow**
```bash
cargo run -- login --openai
```
- Verify browser opens to auth.openai.com
- Complete login
- Verify auth.json has `openai` field

**Step 2: Test model routing**
```bash
cargo run -- chat -m "openai/gpt-4.1"
```
- Verify it uses the OAuth token from auth.json
- Verify streaming works

**Step 3: Commit**
```bash
git add -A && git commit -m "feat: OpenAI integration complete — OAuth login + model routing"
```
