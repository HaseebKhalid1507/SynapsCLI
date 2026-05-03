# Sidecar Protocol v2

Synaps CLI sidecars are long-running plugin processes that communicate with the host over newline-delimited JSON on standard input and output.

The host treats sidecars as generic lego-block processes. Core starts a sidecar, sends generic commands, and consumes generic frames. Plugin-specific semantics stay inside the plugin.

## Transport

- Encoding: UTF-8 JSON Lines.
- One JSON object per line.
- Host writes commands to the sidecar's stdin.
- Sidecar writes frames to stdout.
- Sidecar stderr is reserved for diagnostics.

## Version

The current protocol version is `2`.

A plugin declares its supported sidecar protocol version in its manifest:

```json
{
  "provides": {
    "sidecar": {
      "command": "bin/example-sidecar",
      "protocol_version": 2
    }
  }
}
```

The host also sends the version in the `init` command payload.

## Commands: host to sidecar

### `init`

Sent immediately after process spawn.

```json
{"type":"init","config":{"protocol_version":2}}
```

`config` is an opaque JSON object. Core only reserves `protocol_version`; plugins may define additional keys when supplied by their own bootstrap flow.

### `trigger`

A generic named input from the host.

```json
{"type":"trigger","name":"press"}
{"type":"trigger","name":"release","payload":{"source":"keybind"}}
```

`name` is plugin-defined. The built-in sidecar lifecycle currently uses `press` and `release` for its toggle flow. `payload` is optional and opaque.

### `shutdown`

Requests graceful termination.

```json
{"type":"shutdown"}
```

## Frames: sidecar to host

### `hello`

Initial readiness frame.

```json
{"type":"hello","capabilities":["insert-text","status"]}
```

`capabilities` is a free-form string list. Core does not enumerate or interpret plugin-specific capabilities.

### `status`

Reports a plugin-defined state.

```json
{"type":"status","state":"active","label":"Working"}
{"type":"status","state":"idle"}
```

`state` is free-form. The host treats `idle`, `ready`, and `stopped` as inactive display states; all other states are displayed as active. `label` is optional display text.

### `insert_text`

Requests text insertion into the current input buffer.

```json
{"type":"insert_text","text":"hello world","mode":"final"}
```

Modes:

- `append`: reserved for live-preview style updates.
- `final`: insert finalized text at the cursor.
- `replace`: insert replacement text at the cursor; current host behavior matches `final`.

### `error`

Reports a user-visible sidecar error.

```json
{"type":"error","message":"model file missing"}
```

### `custom`

Plugin-defined extension frame.

```json
{"type":"custom","event_type":"example.event","payload":{"value":1}}
```

Core does not interpret `event_type` or `payload`.

## Compatibility notes

Protocol v2 intentionally has no modality-specific command, frame, capability, or state names. Plugins may expose modality-specific UX through their own lifecycle claim, command names, help text, settings, and internal implementation, but core sidecar protocol fields remain generic.
