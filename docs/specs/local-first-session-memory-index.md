# Phase I — Local-first session memory/index spec

Status: draft for first implementation slice

## Goal

Give SynapsCLI a small local-first session metadata index that helps future
runtime features answer basic questions without parsing full transcripts every
time:

- What sessions happened recently?
- Which workspace/project was each session in?
- Which model/profile was active?
- Which extensions saw session lifecycle hooks?
- What lightweight summary/tags did the user or extensions attach?

This is intentionally **not** a vector memory system, transcript database, or
cross-device sync layer. It is the foundation those systems can optionally read
from later.

## Non-goals

- No semantic embedding pipeline in Phase I slice 1.
- No storage of full user/assistant messages in the metadata index.
- No cloud sync.
- No automatic summarization by an LLM.
- No extension write access to arbitrary memory files.

## Privacy posture

The index is local-only under the Synaps base directory. It stores metadata and
small opt-in notes, not full prompt/tool content. Session lifecycle extension
events must continue to respect existing permissions: extensions receive the
lifecycle event because they requested `session.lifecycle`; they do not gain LLM
content unless they also have the relevant privacy permission on content-bearing
hooks.

## Storage

Default path:

```text
$SYNAPS_BASE_DIR/sessions/index.jsonl
```

Each line is one JSON object. JSONL keeps appends simple and robust against
partial corruption. A later compaction task may rewrite it, but the first slice
only appends.

## Record shape

```json
{
  "schema_version": 1,
  "session_id": "sess-abc123",
  "event": "start",
  "timestamp": "2026-04-30T12:00:00Z",
  "cwd": "/home/user/project",
  "profile": "default",
  "model": "claude-opus-4-6",
  "plugins_enabled": ["policy-bundle"],
  "extensions_loaded": ["policy-bundle"],
  "tags": [],
  "note": null
}
```

End records use `event: "end"` and may include duration/turn counts when known:

```json
{
  "schema_version": 1,
  "session_id": "sess-abc123",
  "event": "end",
  "timestamp": "2026-04-30T12:32:10Z",
  "duration_ms": 1930000,
  "turns": 12,
  "tags": ["debugging"],
  "note": "Fixed extension config validation."
}
```

Required fields for slice 1:

| Field | Type | Description |
|---|---|---|
| `schema_version` | number | Starts at `1`. |
| `session_id` | string | Existing runtime session id. |
| `event` | `start` or `end` | Lifecycle edge represented by this line. |
| `timestamp` | string | UTC RFC3339. |

Optional fields are omitted when unknown. `note` must be short (suggested <= 2
KB) and never auto-filled with raw transcript text.

## Runtime integration

First implementation slice:

1. Add a small `core::session_index` module with append-only JSONL helpers.
2. On session start, append a `start` record before or near firing
   `on_session_start` hooks.
3. On session end, append an `end` record before or near firing
   `on_session_end` hooks.
4. Continue even if index append fails; surface a debug/warn trace but do not
   break chat.

Hook payloads remain unchanged in slice 1. If future slices let extensions attach
session tags/notes, that must use a narrow explicit protocol rather than direct
filesystem mutation.

## Minimal read API

Slice 1 may add pure helpers for tests and future UI:

- `append_record(record)`
- `read_recent(limit)`

No TUI surface is required in slice 1.

## Acceptance criteria for first code slice

- Unit tests cover JSONL append/read using a temp `SYNAPS_BASE_DIR` or explicit
  path helper.
- Start/end record serialization is deterministic enough for tests.
- Index write failures do not abort sessions.
- No raw message/tool content is written by the index module.

## Open questions

- Should `session_id` be the existing runtime id everywhere, or should the index
  mint a separate stable id for resumed sessions?
- Where should user-authored notes/tags come from: slash command, settings UI, or
  extension result on `on_session_end`?
- Should marketplace/plugin trust metadata be snapshotted on session start?
