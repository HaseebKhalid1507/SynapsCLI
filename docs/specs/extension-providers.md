# Extension-provided model providers

Status: Phase N implementation spec

## Objective

Enable installed Synaps plugins to provide local-first model providers through standalone process extensions. A provider extension registers provider/model metadata during `initialize`; Synaps exposes those models in `/models`; when the selected model is an extension model, chat requests route to the extension over JSON-RPC instead of built-in Anthropic/OpenAI-compatible providers.

Success criteria:

- Provider IDs are runtime-namespaced as `plugin-id:provider-id`.
- Model IDs are runtime-namespaced as `plugin-id:provider-id:model-id`.
- Registration remains permission-gated by `providers.register`.
- Non-streaming provider completion is implemented first and adapted to Synaps `StreamEvent` output.
- Streaming is explicitly specified in the contract but may return a clear unsupported error unless a later phase wires notifications.
- Missing required provider config blocks extension load before provider registration.
- `plugin inspect --config` surfaces provider capability/config metadata.
- An example `echo-provider-plugin` demonstrates the full path without network access.

## Commands

SynapsCLI verification:

```bash
cargo test --lib extensions
cargo test --lib runtime::openai
cargo test --bin synaps chatui::models
cargo test --test extensions_e2e
```

Plugin builder verification:

```bash
bash plugin-builder-plugin/scripts/test.sh
```

Example plugin smoke check:

```bash
SYNAPS_BASE_DIR=$(mktemp -d) bash plugin-builder-plugin/scripts/test.sh
```

## Project structure

SynapsCLI:

- `docs/specs/extension-providers.md` — this spec.
- `docs/extensions/contract.json` — protocol/contract source of truth updates.
- `src/extensions/runtime/process.rs` — JSON-RPC provider method calls.
- `src/extensions/runtime/mod.rs` — `ExtensionHandler` provider API.
- `src/extensions/providers.rs` — provider/model registry helpers.
- `src/extensions/manager.rs` — config validation, routing access to handlers.
- `src/runtime/openai/mod.rs` — route extension provider models before built-ins.
- `src/chatui/models/mod.rs` — include extension models in model picker.
- `tests/extensions_e2e.rs` — process fixture provider tests.

synaps-skills:

- `plugin-builder-plugin/contracts/extensions.json` — mirrored contract.
- `plugin-builder-plugin/lib/inspect.sh` — provider metadata/config display.
- `echo-provider-plugin/` — example provider plugin.

## Code style

Prefer small typed request/response structs at protocol boundaries and keep JSON conversion explicit:

```rust
#[derive(Serialize)]
struct ProviderCompleteParams {
    provider_id: String,
    model_id: String,
    messages: Vec<Value>,
    system_prompt: Option<String>,
}

let value = self.call("provider.complete", serde_json::to_value(params)?).await?;
let response: ProviderCompleteResult = serde_json::from_value(value)?;
```

## Testing strategy

- Unit tests for provider/model namespacing and route parsing.
- Process E2E test with a fixture provider extension that returns deterministic text.
- Chat/model UI tests verifying extension model sections and namespaced IDs.
- Builder smoke tests verifying `plugin inspect --config` shows provider details.
- No external network calls in provider tests.

## Boundaries

Always do:

- Keep protocol names/capabilities driven by contract JSON/docs.
- Preserve extension standalone support.
- Fail closed on malformed provider registration or missing required config.
- Namespace provider and model IDs.
- Run targeted tests before commits.

Ask first:

- Adding third-party dependencies.
- Changing extension protocol version.
- Implementing provider override of built-in providers.
- Persisting extension secrets outside existing config/env resolution.

Never do:

- Hardcode specific third-party provider names for extension routing.
- Let an extension register providers without `providers.register`.
- Send prompts to an extension provider unless the selected model belongs to that provider.
- Implement marketplace remote execution or auto-enable trust.

## Provider routing semantics

### Identity

An extension registers provider-local IDs during `initialize`:

```json
{
  "id": "echo",
  "models": [{ "id": "echo-small", "display_name": "Echo Small" }]
}
```

Synaps stores/exposes:

- Runtime provider ID: `plugin-id:echo`
- Runtime model ID: `plugin-id:echo:echo-small`

The `:` delimiter is reserved for extension model IDs. Provider IDs and model IDs must not contain `:`.

### Config resolution

Provider config uses existing `extension.config` entries in the plugin manifest. Synaps resolves env/config/defaults before `initialize` and passes `params.config` to the extension. If an extension declares provider config requirements in `capabilities.providers[].config_schema.required`, every required key must be present in `params.config`; otherwise extension load fails and no provider is registered.

### JSON-RPC methods

`provider.complete` request:

```json
{
  "provider_id": "echo",
  "model_id": "echo-small",
  "model": "plugin-id:echo:echo-small",
  "messages": [],
  "system_prompt": "optional",
  "tools": [],
  "temperature": null,
  "max_tokens": null,
  "thinking_budget": 0
}
```

`provider.complete` result:

```json
{
  "content": [{ "type": "text", "text": "hello" }],
  "stop_reason": "end_turn",
  "usage": { "input_tokens": 1, "output_tokens": 1 }
}
```

`provider.stream` is reserved in the contract for later notification-based streaming. Phase N routes chat through `provider.complete` and emits returned text as stream events.

### Failures/timeouts/cancellation

- `provider.complete` has a bounded timeout (60s in Phase N).
- Cancellation is checked before and after the call. In-flight JSON-RPC process calls may finish in the background; Synaps returns canceled to the chat caller.
- Malformed provider responses return a config/runtime error and do not fall back to Anthropic.
- Extension transport failures use the same restart policy as tools/hooks.

### Phase O notes

Provider trust and distribution metadata are now part of the platform surface:

- `providers.register` is high impact. Install/update/detail UX should explain that selected extension-provider models receive conversation content.
- `provider.complete` is the only executable provider call in this phase.
- `provider.stream` is reserved and implementations return a clear unsupported error until notification-based streaming semantics are specified.
- Plugin index/package metadata may include provider summaries derived from manifest `extension.providers` metadata. Runtime registration remains authoritative because extensions return actual capabilities from `initialize`.

### Trust/security

Provider extensions can receive conversation content when selected. `providers.register` is high impact and install/inspect UX must expose it. Secrets are resolved by Synaps and passed only via initialize config; extensions must not persist them on behalf of Synaps.
