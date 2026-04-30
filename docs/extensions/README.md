# SynapsCLI Extensions

Extensions are external processes that hook into SynapsCLI's runtime. They observe and intercept events — tool calls, messages, sessions — and can modify behavior without touching core source code.

This document covers everything you need to **use** extensions. If you want to **build** one, see [protocol.md](./protocol.md), [hooks.md](./hooks.md), and [permissions.md](./permissions.md).

---

## What Are Extensions

An extension is an independent process that SynapsCLI spawns alongside the main runtime. It communicates over stdio using a lightweight JSON-RPC protocol, and declares which events it wants to receive.

Extensions can:

- **Observe** — log, audit, or monitor tool calls and messages
- **Block** — prevent a `before_tool_call` event from proceeding
- **Inject** — prepend context into the system prompt before a request reaches the LLM

Future protocol phases reserve names for tool/provider registration, but phase 1 does not grant those capabilities yet.

SynapsCLI spawns each extension as a subprocess on session start and tears it down on session end. Extensions run with the plugin root as their current working directory. If an extension crashes or times out, the runtime fails open — the event proceeds as if the extension wasn't there. Transport failures trigger up to three restart attempts before the extension is marked failed.

---

## Installing Extensions

Extensions live under the user plugin directory (`~/.synaps-cli/plugins/`) and may also live under a project-local directory (`./.synaps/plugins/`). Each plugin gets its own directory named after the extension:

```
~/.synaps-cli/plugins/
  my-auditor/
    .synaps-plugin/
      plugin.json
    main.py
./.synaps/plugins/
  project-auditor/
    .synaps-plugin/
      plugin.json
    index.js
```

To install an extension from a git repository:

```bash
git clone https://github.com/example/my-auditor ~/.synaps-cli/plugins/my-auditor
```

SynapsCLI scans both locations on startup. Any subdirectory containing a `.synaps-plugin/plugin.json` with a valid `extension` field is loaded automatically. If the same plugin directory name exists in both locations, the project-local plugin in `./.synaps/plugins/` overrides the user plugin for that project.

---

## Plugin Structure

Every extension must include a manifest at `.synaps-plugin/plugin.json`. The manifest declares metadata, the process command, requested permissions, and which hooks to subscribe to.

```json
{
  "name": "my-auditor",
  "version": "0.1.0",
  "description": "Logs all tool calls to a local audit file.",
  "author": "Your Name",
  "extension": {
    "protocol_version": 1,
    "runtime": "process",
    "command": "python3",
    "args": ["main.py"],
    "permissions": ["tools.intercept"],
    "hooks": [
      { "hook": "before_tool_call" },
      { "hook": "after_tool_call" }
    ]
  }
}
```

The `extension` field is what distinguishes a plugin that provides an extension from one that only declares tools or themes. Its fields:

| Field              | Type            | Description                                              |
|--------------------|-----------------|----------------------------------------------------------|
| `protocol_version` | integer         | Extension protocol version; phase 1 uses `1`             |
| `runtime`          | string          | Runtime type; phase 1 supports `process` only            |
| `command`          | string          | Executable or plugin-relative script path to launch      |
| `args`             | array           | Arguments passed to `command`; local files resolve from the plugin dir when safe |
| `permissions`      | array of string | Permissions the extension requires to function correctly |
| `hooks`            | array           | List of hook subscriptions (see below)                   |

---

## Available Hooks

| Hook                | Fires when…                                              | Can block? | Can confirm? | Can inject? |
|---------------------|----------------------------------------------------------|------------|--------------|-------------|
| `before_tool_call`  | A tool is about to be executed                           | ✅          | ✅            | ❌           |
| `after_tool_call`   | A tool has finished executing                            | ❌          | ❌            | ❌           |
| `before_message`    | A user message is about to be sent to the model          | ❌          | ❌            | ✅           |
| `on_session_start`  | A new session has been initialized                       | ❌          | ❌            | ❌           |
| `on_session_end`    | A session is being torn down                             | ❌          | ❌            | ❌           |

**Notes:**

- `before_tool_call` supports `block`; if any extension blocks, the tool is not executed and later handlers are skipped.
- `before_tool_call` also supports `confirm`, which requests explicit user approval before proceeding. Interactive TUI streams prompt the user; headless/non-interactive call sites fail closed by blocking the tool call.
- `before_message` supports `inject`; injected content from matching extensions is accumulated.
- Other hooks are observation-oriented today. Returning an unsupported action is ignored by the current call site.

---

## Tool-Specific Hooks

You can narrow a hook registration to a specific tool by adding a `"tool"` field:

```json
"hooks": [
  { "hook": "before_tool_call", "tool": "bash" },
  { "hook": "before_tool_call", "tool": "read_file" }
]
```

Your extension will only receive `before_tool_call` events for the named tools. Events from all other tools are filtered out before reaching your process — you won't see them, and you won't need to handle them.

This is the recommended approach when your extension only cares about specific tools. It reduces noise and avoids unnecessary inter-process communication.

Omitting the `"tool"` field registers a wildcard — your extension receives that hook for every tool.

---

## Permissions

Extensions must declare the permissions they require. SynapsCLI rejects unknown permission strings and refuses hook subscriptions that lack the hook's required permission.

| Permission           | What it grants                                                                 |
|----------------------|--------------------------------------------------------------------------------|
| `tools.intercept`    | Ability to receive `before_tool_call` / `after_tool_call` events               |
| `privacy.llm_content`| Access to message content for `before_message`                                 |
| `session.lifecycle`  | Receipt of `on_session_start` and `on_session_end` events                      |

Reserved future permissions are documented in [permissions.md](./permissions.md) but are rejected if declared today: `tools.override`, `tools.register`, and `providers.register`.

Permissions are checked before events are delivered. An extension that lacks a hook's required permission is not subscribed to that hook.

---

## Context Injection

When an extension returns a `HookResult::Inject` from a supported hook, the provided content is prepended to the system prompt before it reaches the LLM. This allows extensions to dynamically augment the assistant's context — for example, injecting the current user's timezone, a relevant memory, or a policy statement.

```json
{
  "action": "inject",
  "content": "The current user is in UTC+5:30. Prefer IST when discussing times."
}
```

Injected content from multiple extensions is concatenated in load order, separated by blank lines. Injected content is not stored in session history — it applies only to the current request.

---

## Configuration

### Disabling Extensions

To start SynapsCLI with all extensions disabled:

```bash
synaps --no-extensions
```

This is useful for debugging when you want to isolate whether behavior is coming from core or an extension.

### Environment Variables

| Variable      | Description                                                                              |
|---------------|------------------------------------------------------------------------------------------|
| standard tracing/log configuration | Extension lifecycle and hook errors are emitted through SynapsCLI tracing |

---

## Built-in Extensions

The official extension collection is maintained in the [synaps-deck](https://github.com/synaps-cli/synaps-deck) repository. It includes:

- **audit-log** — writes all tool calls and results to a structured JSONL file
- **confirm-shell** — prompts for human confirmation before any `bash` execution
- **memory-injector** — injects relevant memories from a local store into the system prompt
- **rate-limiter** — blocks tool calls that exceed a configurable per-minute threshold

Install any of them individually:

```bash
git clone https://github.com/synaps-cli/synaps-deck ~/.synaps-cli/plugins/synaps-deck
```

Each subdirectory in `synaps-deck` is a self-contained extension with its own manifest.

---

## Writing Your Own Extension

Extensions communicate with SynapsCLI over stdio using JSON-RPC 2.0 with Content-Length framing. Any language that can read from stdin and write to stdout can implement an extension.

See [protocol.md](./protocol.md) for the full technical specification, including:

- Exact wire format and framing
- Method signatures and schemas
- A complete working Python example
- Error handling expectations
