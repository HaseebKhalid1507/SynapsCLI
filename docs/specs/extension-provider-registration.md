# Extension provider registration design

Status: implemented metadata registry slice. `providers.register` is active; provider chat routing remains future work.

## Goals

- Let a trusted plugin extension register local-first model/provider integrations at runtime.
- Keep provider registration permissioned, inspectable, and reversible.
- Preserve SynapsCLI control of credential storage, config resolution, routing, and UI selection.
- Avoid hardcoded provider names in the extension runtime; providers are capabilities declared over protocol.

## Non-goals

- No remote marketplace execution without local install/trust.
- No extension-managed secret persistence.
- No multi-process transport multiplexing in Phase 1.
- No provider override of built-in providers until `providers.override` or an equivalent policy exists.

## Permission model

- `providers.register` is active. An extension must declare it before returning provider capabilities from `initialize`.
- An extension that returns provider capabilities from `initialize` without `providers.register` fails closed and is shut down.
- Provider registration is independent from hooks and tools; hookless provider-only extensions are valid when `providers.register` is declared.
- Future override behavior must be a separate permission. Registered provider IDs must not collide with built-ins or already-loaded providers.

## Protocol

`initialize` result adds an optional provider capability:

```json
{
  "protocol_version": 1,
  "capabilities": {
    "providers": [
      {
        "id": "local-llama",
        "display_name": "Local Llama",
        "description": "Local OpenAI-compatible endpoint",
        "models": [
          {
            "id": "llama-3.1-8b",
            "display_name": "Llama 3.1 8B",
            "capabilities": {
              "streaming": true,
              "tool_use": true,
              "vision": false,
              "reasoning": false
            },
            "context_window": 131072
          }
        ],
        "config_schema": {
          "type": "object",
          "properties": {
            "base_url": {"type": "string", "format": "uri"},
            "api_key_env": {"type": "string"}
          },
          "required": ["base_url"]
        }
      }
    ]
  }
}
```

Provider IDs are namespaced at registration as `plugin-id:provider-id`. Model IDs exposed to routing are `plugin-id:provider-id/model-id` unless a later registry chooses a different display alias.

## Runtime calls

The extension protocol should add methods only after the registry is designed:

- `provider.list_models` may refresh model metadata for dynamic providers.
- `provider.chat` handles a single model request and returns the same internal streaming/non-streaming shape Synaps uses for providers.
- `provider.cancel` is optional and tied to a request ID if streaming cancellation is needed.

Phase 1 provider transport can be non-multiplexed like tool calls: one serialized request/response per process. Streaming may be emulated with newline JSON-RPC notifications only if explicitly specified; otherwise start with non-streaming to avoid transport complexity.

## Config and secrets

- Provider capabilities may include `config_schema`; actual config is resolved by Synaps from user/project config and passed into `initialize`.
- Secrets are referenced by env var name or Synaps secret handle, never persisted in plugin manifests.
- The inspect UI must show requested provider IDs, config keys, and secret references before enable.

## Validation

Reject provider specs when:

- `id`, `display_name`, or `description` is empty.
- `id` is not lower-kebab/identifier-safe.
- `models` is empty.
- model IDs are empty or duplicated within a provider.
- `config_schema` exists and is not a JSON object.
- provider ID collides with built-ins or already-loaded extension providers.

## Lifecycle

1. Discover plugin manifest.
2. Validate manifest permissions.
3. Spawn extension.
4. `initialize` with resolved config.
5. Validate provider specs.
6. Permission-gate `providers.register`.
7. Register providers into provider registry.
8. If any post-init validation fails, shut down the extension and remove partial registrations.
9. On unload/shutdown, unregister providers before killing process.

## Trust and inspection

`plugin inspect` / `/extensions` should show:

- requested permissions, including `providers.register`;
- provider IDs and models returned by initialize where available;
- config keys and secret references;
- whether provider IDs would collide.

Install/enable prompts should treat provider registration as high impact because it can receive prompts, conversation history, and tool results depending on selected model usage.

## Rollout

1. Spec + contract placeholders only.
2. Config story lands (E2), so initialize has resolved config.
3. Trust inspection lands (E3).
4. `providers.register` is active in the contract.
5. Provider capabilities are parsed and validated.
6. Provider metadata is registered without routing calls.
7. Implement non-streaming `provider.chat`.
8. Add streaming/cancel only after Phase 1 proves stable.
