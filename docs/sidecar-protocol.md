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

Sent after the sidecar’s versioned Hello is accepted.

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

Initial protocol-readiness frame, emitted before the host sends Init.

```json
{"type":"hello","protocol_version":2,"extension":"example","capabilities":["insert-text","status"]}
```

`capabilities` is a free-form string list. The optional host capability
`ready_after_init` opts into a stronger initialization contract: after processing
Init and completing model/device loading, the sidecar emits exactly
`{"type":"status","state":"ready"}`. It must not emit this acknowledgment before
Init. Generic `status`, or an `idle`/`stopped` status, does not make that promise.
The TUI keeps such a sidecar loading until that acknowledgment; error, exit, or
timeout fails startup instead of activating it. Without this capability,
Hello plus a successful Init write remains the legacy protocol-ready boundary;
it is not a guarantee that plugin-specific initialization has finished.

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

## Non-blocking TUI startup and activation

Extension discovery/loading already runs in the background after terminal setup.
Sidecar processes remain on-demand: no prewarming of disabled/deferred plugins
or automatic device activation. The first explicit toggle starts an owned
background task for plugin bootstrap RPC and the sidecar handshake. It returns
immediately with “still loading — try the toggle again when ready”. Repeated
toggles/status requests do not wait, create duplicate processes, or queue a press.
Completion leaves the sidecar **unarmed** and reports “ready — toggle to activate”.

The UI uses the cached, disable-filtered plugin registry instead of rediscovering
plugins on every toggle/status. Extension commands/settings reads do not wait on
an extension loader’s manager lock; they report loading/busy and can be retried.
Sidecar startup has a 30-second overall deadline, including the bootstrap lock/RPC
and optional readiness acknowledgment. Hello is bounded to 10 seconds and each
command write to 2 seconds. A partial failed/timed-out/cancelled write closes the
input pipe rather than permitting a corrupt-frame retry.

Failures clear loading state for retry. Plugin disable/removal/reload cancels
ineligible pending startups and drops live instances. Shutdown aborts owned
startup tasks and drops children; a late completion cannot resurrect a removed
plugin. Queued lifecycle state is checked again before publishing readiness or
sending a trigger.

This removes sidecar startup waits from the input loop. It does not eliminate
other pre-render work such as session loading, filesystem discovery, repository
identity checks, or terminal negotiation. Changes take effect in a newly built
Synaps process; an already-running executable is not hot-patched.
