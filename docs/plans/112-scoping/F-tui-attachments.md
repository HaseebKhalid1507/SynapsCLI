# F — Multimodal attachments: TUI / client half scoping

## §1  Situation

The engine-side attachment infrastructure landed in the #120 engine merge
(phases 7–8) and is live on `origin/dev`.  What exists:

| Component | Location on `origin/dev` | Status |
|---|---|---|
| `PendingAttachments`, `load_attachment`, `build_user_content` | `crates/agent-engine/src/attachments.rs:1–253` | **Identical** to upstream; fully functional |
| `validate_messages` / `validate_tool_blocks` / `bounded_tool_results` | `crates/agent-engine/src/runtime/attachments.rs:1–380+` | **Identical**; model+transport gating with modality evidence |
| `attachment_path_argument`, `ATTACHMENT_COMMANDS`, `ATTACHMENT_DISCLOSURE` | `crates/agent-engine/src/skills/registry.rs:144–167` | **Identical**; path parsing + reserved command names |
| `SessionCommand::Submit { text, attachments }` | `crates/agent-engine/src/session/types.rs:460–467` | Wire type present; **`attachments` silently ignored by actor** (`:2169`: `Submit { text, .. }`) |
| `RpcAttachment` | `crates/agent-core/src/core/rpc_protocol.rs:61–73` | Path-based wire type; present on both branches |
| Request-time `validate_messages` gate | `crates/agent-engine/src/runtime/openai/mod.rs:262` | Live; blocks unsupported media before any provider call |
| `HISTORY_IMAGE_BYTE_CAP` / `cap_history_image_bytes` | `crates/agent-engine/src/runtime/stream.rs:1965–1990` | Live; degrades oldest image blocks above 20 MiB |
| `user_content_for_display` | `crates/agent-core/src/core/session.rs:75–120` | Live; projects `[attached image]` / `[attached document: X]` labels |
| `docs/multimodal.md` | `docs/multimodal.md` | **Present and complete** on dev |
| `docs/specs/multimodal-attachments.md` | `docs/specs/multimodal-attachments.md` | **Present and complete** on dev |

What is **missing** is everything between the user's `/attach` keystroke and the
actor's `Submit` handler — the TUI surface, the headless `synaps chat` surface,
and the RPC attachment loader.  Upstream built all three against `Runtime`
in-process; dev's TUI is a thin client of the `SessionActor`.

---

## §2  Inventory — upstream symbols, disposition

### §2.1  `crates/agent-tui/src/tui/app.rs`

| Symbol | Upstream lines | What it does | Disposition |
|---|---|---|---|
| `App::pending_attachments: PendingAttachments` | app.rs field (upstream `:37`) | Client-local staging buffer for captured bytes | **port-as-render** — lives on `App`, never crosses the wire |
| `App::append_user_submission(model, input)` | app.rs `:372–401` | Builds multipart content from pending + text, runs `validate_messages`, pushes to `api_messages`, clears pending | **needs-decision** — upstream pushes to local `api_messages`; dev has no `api_messages` on the client. See §3 |
| `App::api_messages` | app.rs field (upstream `:35`) | Full history copy in app | **deliberate-skip** — dev removed this in the daemon migration; client holds `api_messages_len` only (`dispatch.rs:1231`) |

### §2.2  `crates/agent-tui/src/tui/input.rs`

| Symbol | Upstream lines | What it does | Disposition |
|---|---|---|---|
| `handle_key` Enter guard: `!app.pending_attachments.is_empty()` | input.rs `:370–380` | Allows Enter to submit when text is empty but attachments staged | **port-as-render** — pure input guard on `App` state |
| `process_submit` compaction+attachment guard | input.rs `:458–464` | Rejects submit during compaction when attachments are pending | **port-as-render** — dev uses `app.compacting` bool (from actor); same guard shape |
| `process_streaming_submit` attachment guard | input.rs `:493–497` | Rejects streaming submit with pending attachments | **port-as-render** — pure client-side check |
| `attachment_only_enter_and_busy_draft_retention` (test) | input.rs `:1149–1196` | Verifies attachment-only Enter + draft retention on reject | **port-as-render** — test; adapts to dev's `app.compacting` bool |
| `splitn(2, char::is_whitespace)` | input.rs `:479` | Whitespace-aware command parsing (vs `' '` only) | **port-as-render** — minor fix, no actor involvement |

### §2.3  `crates/agent-tui/src/tui/commands.rs`

| Symbol | Upstream lines | What it does | Disposition |
|---|---|---|---|
| `handle_pending_attachment_command(cmd, arg, app)` | commands.rs `:19–51` (upstream diff) | Handles `/attachments` list and `/detach` clear — pure `App` mutation | **port-as-render** — no wire involvement |
| `handle_command` `/attach` arm | commands.rs `:155–180` (upstream diff) | Calls `attachment_path_argument` → `load_attachment` → `app.pending_attachments.add` | **port-as-render** — file I/O is client-local; bytes never leave the TUI process until Submit |
| `handle_command` resume/new/compact guard | commands.rs `:155–158` (upstream diff) | Refuses session-switch commands while attachments staged | **port-as-render** |

### §2.4  `crates/agent-tui/src/tui/helpers.rs`

| Symbol | Upstream lines | What it does | Disposition |
|---|---|---|---|
| `rebuild_display_messages` (upstream version) | helpers.rs `:148–230` (upstream) | Walks `api_messages` inline, calls `user_content_for_display` | **already-on-dev-as** `rebuild_display_messages` / `apply_display_tail` — dev uses `DisplayTail` from the actor; `user_content_for_display` already handles attachment labels. **No port needed.** |
| `rebuild_display_projects_attachments_without_tool_result_prompts` (test) | helpers.rs `:348–400` (upstream) | Asserts image/document labels are projected, sentinels excluded | **already-on-dev** — the test uses `user_content_for_display` from `agent-core` which is shared and already handles attachment content types |

### §2.5  `crates/agent-tui/src/tui/dispatch.rs`

| Symbol | Upstream lines | What it does | Disposition |
|---|---|---|---|
| `Submit` arm attachment handling | dispatch.rs `:1222–1308` (upstream) | Guards attachment+busy, calls `app.append_user_submission`, builds display with summaries | **port-as-actor-cmd** — the critical site; see §3 |
| `StreamingInput` attachment commands | dispatch.rs `:1341–1349` (upstream) | Routes `/attachments`/`/detach` while streaming, blocks other slash+attachment combos | **port-as-render** |

### §2.6  `src/cmd/chat.rs` (headless)

| Symbol | Upstream lines | What it does | Disposition |
|---|---|---|---|
| `append_user_submission(conv, pending, model, text)` | chat.rs `:49–75` | Same as app.rs version but on `ConversationState` | **needs-decision** — dev's `chat.rs` is actor-based (`mod actor`); see §3.3 |
| `list_pending_attachments(pending)` | chat.rs `:76–88` | stderr listing of staged files | **port-as-render** — pure eprintln, client-local |
| `/attach`, `/attachments`, `/detach` arms | chat.rs `:343–390` | Slash commands in the headless stdin loop | **port-as-render** + actor Submit shape |

### §2.7  `src/cmd/rpc.rs` (stdio RPC)

| Symbol | Upstream lines | What it does | Disposition |
|---|---|---|---|
| `rpc_attachment_paths(attachments)` | rpc.rs diff `:+565–585` | Validates absolute paths, no `..` | **port-as-render** — RPC runs in-process even on dev |
| `load_rpc_user_content(message, attachments)` | rpc.rs diff `:+587–595` | Calls `build_user_content` after path validation | **port-as-render** — same process; no daemon |
| `handle_prompt` attachment branch | rpc.rs diff `:+619–720` | Lock, load, validate_messages, append, emit disclosure | **port-as-render** — RPC drives `Runtime` in-process (same on dev); not a daemon session |

---

## §3  The actor question — wire shape decision

### §3.1  The problem

Upstream's TUI holds `app.api_messages` and calls `validate_messages(model,
&proposed)` with the full history plus the new attachment message *before*
accepting the submission.  Dev's TUI has **no** `api_messages` — the actor
owns the journal (`dispatch.rs:1231–1253` sends `SessionCommand::Submit {
text, attachments: Vec::new() }`).

Under the daemon, `load_attachment` reads **the client's filesystem**.  The
daemon's cwd and fs are not the client's.  `--system` path resolution (T4)
already solves this client-side for exactly this reason.  Attachments must
follow the same principle.

### §3.2  Decision: client loads bytes, ships content blocks

Three alternatives were evaluated:

| Alternative | Wire payload | Who loads | Who validates | Verdict |
|---|---|---|---|---|
| A. Client sends paths → actor reads | `RpcAttachment { path }` | Actor | Actor | **Rejected** — daemon fs ≠ client fs; breaks under socket transport, `synaps attach` from another machine, containerized daemon |
| B. Client loads + ships raw bytes | New `WireAttachment { name, mime, data: Vec<u8> }` | Client | Actor | Feasible but doubles serde overhead (base64 in JSON-over-UDS); 64 MiB `DAEMON_MAX_FRAME_BYTES` (`wire.rs:41`) is sufficient |
| C. Client loads, builds canonical content blocks, ships them in Submit | `Submit { text, content_blocks: Vec<Value> }` | Client | Actor (re-validates) | **Recommended** — client calls `PendingAttachments::build_content` exactly as upstream does, then sends the pre-built `Value`; actor validates with `validate_messages` before appending |

**Recommended wire shape (Alternative C):**

```rust
// session/types.rs — SessionCommand
Submit {
    text: String,
    /// Pre-built canonical user content blocks (images, documents)
    /// produced by `PendingAttachments::build_content` on the client.
    /// Empty ⇔ text-only (backward compatible; existing `Vec::new()` unchanged).
    #[serde(default)]
    attachments: Vec<serde_json::Value>,
},
```

The actor receives `Submit { text, attachments }` and:
1. If `attachments` is empty → current path (push `{"role":"user","content": text}`).
2. If non-empty → build `content = attachments` array (text block prepended if
   non-empty), validate with `validate_messages(model, &proposed)`, reject with
   `SystemNotice` on failure, push on success.

**Why this works:**
- Client calls `load_attachment` (its own fs) → `PendingAttachments::add` →
  on Submit, `build_content` → canonical `Value` blocks already in the JSON
  shape `validate_messages` expects.
- Actor re-validates because the model may have changed between `/attach` and
  Enter, or because the history context changed (compaction, resume).
- Wire cost: the `Value` blocks serialize directly into the JSON frame.
  A 3.5 MiB image → ~4.7 MiB base64 in JSON — well within the 64 MiB frame cap.
  Multiple large images may approach the cap; the `MAX_TOTAL_BYTES` (15 MiB raw,
  `attachments.rs:12`) and `MAX_HISTORY_ENCODED_BYTES` (20 MiB,
  `runtime/attachments.rs:27`) limits fire first.
- `HISTORY_IMAGE_BYTE_CAP` (20 MiB, `stream.rs:1965`) is a per-request
  degradation cap, not a submission cap — it drops oldest image blocks at request
  time.  No interaction with the wire shape.
- Backward compatible: existing `attachments: Vec::new()` callers (dispatch.rs,
  chat.rs actor) send an empty vec; serde `#[serde(default)]` handles missing field.

**Existing `RpcAttachment` in the `Submit` variant** (`types.rs:466`) uses path-based
`agent_core::core::rpc_protocol::RpcAttachment`.  This must be replaced with
`Vec<serde_json::Value>` (content blocks).  The `RpcAttachment` type remains for
the stdio RPC protocol (`rpc_protocol.rs:61`) where the client IS the process and
paths are meaningful.

### §3.3  Per-surface split

| Surface | Loads bytes | Builds content | Ships via | Actor validates |
|---|---|---|---|---|
| **TUI** (`dispatch.rs`) | Client: `/attach` → `load_attachment` (client fs) | Client: `build_content` on Submit | `SessionCommand::Submit { text, attachments: Vec<Value> }` | Yes |
| **Headless chat** (`src/cmd/chat.rs` actor mode) | Client: `/attach` → `load_attachment` | Client: `build_content` | `SessionCommand::Submit` via `LocalTransport` (in-process, zero copy) | Yes |
| **Stdio RPC** (`src/cmd/rpc.rs`) | In-process: `load_rpc_user_content` | In-process: `build_user_content` | Direct `api_messages.push` (no actor — rpc drives Runtime directly) | In-process `validate_messages` |
| **`synaps send`** (`src/cmd/send.rs`) | N/A — text-only | N/A | `SessionCommand::Submit { text, attachments: vec![] }` | N/A |

### §3.4  Model capability gating

`validate_messages` (`runtime/attachments.rs:49`) resolves a `Transport` per model
(`:117–175`) checking `WireProtocol`, `AuthPolicy`, and `Modality` evidence from
the capability cache or static Codex catalog.  The error messages are model-neutral
("Selected model lacks exact image input capability metadata").

The TUI learns "this model can't take images" **at submit time**, not at `/attach`
time — which is correct because:
1. The user may switch models between `/attach` and Enter.
2. `/attach` captures bytes; it does not promise they will be accepted.
3. The actor re-validates, catching model changes that race with the submission.

On rejection, the actor emits `SystemNotice` with the validation error.  The
client retains `pending_attachments` (they are not cleared until successful
submission).  The user can `/model` switch and retry, or `/detach`.

This matches upstream's TUI behaviour exactly (`dispatch.rs:1258–1264` upstream:
`app.append_user_submission` returns `Err`, attachments retained, error pushed).

---

## §4  Phase plan

All phases are **DARK** — no behaviour change unless the user explicitly runs
`/attach`.  The attachment commands are already registered in the `CommandRegistry`
(`ATTACHMENT_COMMANDS`, `skills/registry.rs:144`) and reserved against plugin
hijacking on dev today.  They simply respond "unknown command" until the
TUI handlers are wired.

| # | Size | Hours | Description | Tests | DARK rationale |
|---|---|---|---|---|---|
| F1 | S | 2 | **Wire shape**: change `SessionCommand::Submit.attachments` from `Vec<RpcAttachment>` to `Vec<serde_json::Value>` in `session/types.rs:466`. Update actor `submit()` (`actor.rs:1149–1192`) to handle non-empty attachments: build content, `validate_messages`, reject with `SystemNotice`. Update the `Debug` impl (`types.rs:543–547`) and serde round-trip test (`types.rs:1048`). | Extend round-trip test. New unit test: Submit with attachment blocks → actor validates → appends. Submit with unsupported model → actor rejects → `SystemNotice`. | All existing callers send `Vec::new()` → no behaviour change. |
| F2 | S | 2 | **TUI `/attach` + `/attachments` + `/detach`**: Add `pending_attachments: PendingAttachments` field to `App` (`app.rs`; type is already `Default`). Port `handle_pending_attachment_command` into `commands.rs`. Port `/attach` arm from upstream `commands.rs` into dev's `handle_command`. Port idle guards from upstream `input.rs` Enter paths (`:370–380`, `:458–464`, `:493–497`). | Port `attachment_only_enter_and_busy_draft_retention` test (adapted for `app.compacting` bool). New test: `/attach` path parsing, error on missing file, `/detach` clears, `/attachments` lists. | `/attach` only captures bytes client-side; no wire traffic until Submit. |
| F3 | M | 3 | **TUI Submit with attachments**: In `dispatch.rs` `InputAction::Submit` arm (`:1227–1253`), replace `attachments: Vec::new()` with `app.pending_attachments.build_content` → `Vec<Value>` blocks when pending is non-empty. Add display text with `[Attachments]` summaries (upstream `:1249–1257`). Add attachment-busy guards (upstream `:1228–1236` adapted for `app.compacting`). Wire `StreamingInput` attachment command routing (upstream `:1341–1349`). Handle `Refused` by restoring `last_submitted` + retaining pending. | Integration test: submit with staged image → actor receives content blocks → turn starts. Submit during streaming → rejected, attachments retained. Submit with unsupported model → rejected, editor text restored. | Requires F1 (wire) + F2 (capture). The Submit path only activates when `pending_attachments` is non-empty. |
| F4 | S | 2 | **Headless chat attachments**: In `src/cmd/chat.rs` actor mode (`:695+`), add `PendingAttachments` local, `/attach`/`/attachments`/`/detach` in the stdin command parser (`:1150` area), and the Submit attachment shape (`:1161–1163`). Port piped-input fail-stop. Headless attachment-only blank-line submit. | New test module: piped `/attach` + message → turn with attachments. Piped `/attach` with missing file → fail-stop. | `synaps chat` is rarely used in production; attachments are opt-in via `/attach`. |
| F5 | M | 3 | **RPC attachment loading**: Port `rpc_attachment_paths` and `load_rpc_user_content` into `src/cmd/rpc.rs`. Replace the `build_user_content` (text-only path placeholder, `rpc_dispatch.rs:191`) call with real content loading via `agent_engine::attachments::build_user_content`. Add session-busy recheck, `validate_messages` gate, and `attachments.disclosure` response event. | Port upstream's `mod attachment_tests`. New: RPC prompt with image path → content loaded → disclosure emitted → turn completes. RPC prompt with `..` path → error. | RPC attachment protocol is already documented in `docs/multimodal.md`. The stdio bridge caller must opt in by sending `attachments: [...]`. |
| F6 | S | 1 | **Polish + symbol audit**: Run `scripts/merge/symbol-audit.py`. Verify all upstream attachment symbols are accounted for. Confirm `docs/multimodal.md` accuracy against the actor-based paths. Update `LEDGER.md`. | Workspace build on bella. Full test suite. | — |

**Total: 13 hours base, ~16 hours with buffer.**

### Dependency graph

```
F1 (wire) ──→ F3 (TUI submit)
                  ↑
F2 (capture) ─────┘
F4 (chat) depends on F1
F5 (rpc) is independent (rpc drives Runtime directly, no actor)
F6 (audit) depends on F1–F5
```

F2 and F5 can run in parallel.  F1 must land first to unblock F3 and F4.

---

## §5  Risks

| # | Risk | Severity | Mitigation |
|---|---|---|---|
| R1 | **Frame size under socket transport**: a single 3.5 MiB image → ~4.7 MiB base64 in JSON; 8 images → ~37 MiB.  `DAEMON_MAX_FRAME_BYTES` is 64 MiB (`wire.rs:41`). | Low | `MAX_TOTAL_BYTES` (15 MiB raw, `attachments.rs:12`) fires before the frame cap.  Worst case: 15 MiB raw → ~20 MiB base64 + JSON overhead < 64 MiB.  No change needed. |
| R2 | **Digest-mode clients** (phase 4 thin client): `Attached.api_messages = []` in digest mode.  If the actor sends a `Conversation` snapshot after an attachment submit, the digest client has no local history to display the attachment label. | Low | `user_content_for_display` runs in `display_tail` on the daemon side (`session/display.rs`), which projects attachment labels server-side.  Digest clients receive `DisplayItem::User { text }` with labels already embedded.  No issue. |
| R3 | **Model switch between `/attach` and Enter**: user stages an image, switches to a text-only model, presses Enter. | None | Actor re-validates with current model.  `validate_messages` returns `Err`, actor emits `SystemNotice`, client retains attachments.  Matches upstream behaviour. |
| R4 | **Compaction of attachment messages**: compaction summarizes history; image/document source bytes in summarized messages are lost. | None | By design — `docs/multimodal.md` documents this.  `user_content_for_display` in the compaction path (`runtime/compaction.rs:156,460`) projects labels, not bytes.  The summary carries `[attached image]` / `[attached document: X]`.  Provider requests after compaction cannot re-send the image — this is expected and documented. |
| R5 | **`RpcAttachment` type change in `Submit`**: changing from `Vec<RpcAttachment>` to `Vec<Value>` is a breaking wire change if any external consumer sends `Submit` with attachment paths over the socket. | Very low | No external consumer does this today — the field is `#[serde(default)]` and all callers send `Vec::new()`.  The `RpcAttachment` type remains for the stdio RPC protocol.  A protocol version bump is not needed because the field has never carried non-empty values over the session protocol. |

---

## §6  Open questions (≤ 3, need a human)

1. **Q1 — Content blocks vs. a dedicated `WireAttachment` type?**
   §3.2 recommends `Vec<serde_json::Value>` (canonical content blocks) in `Submit`.
   The alternative is a purpose-built `WireAttachment { name: String, mime: String,
   data: Vec<u8> }` that the actor re-encodes into canonical blocks — more type
   safety but double the serde cost and a new type to maintain.
   **Recommendation:** `Vec<Value>`.  The client already builds canonical blocks
   via `PendingAttachments::build_content`; the actor already validates them via
   `validate_messages` which expects `Value`.  A typed wrapper adds ceremony
   without safety — the `Value` is validated structurally by `validate_attachment`
   (`runtime/attachments.rs`).
   **Decision needed:** Confirm `Vec<Value>` or request the typed wrapper.

2. **Q2 — Headless chat (`synaps chat`) attachment priority?**
   F4 ports attachments to the actor-mode headless chat.  This is ~2 hours and
   maintains feature parity documented in `docs/multimodal.md`.  However,
   headless chat usage is low and the legacy inline loop (behind
   `#[cfg(feature = "legacy_inline")]`, `chat.rs:15`) already has no attachment
   support on dev.
   **Decision needed:** Port now (F4) or defer to a later sprint?

3. **Q3 — RPC attachment path validation hardening?**
   Upstream's `rpc_attachment_paths` (`rpc.rs`) validates absolute + no `..` but
   does not restrict to a project sandbox.  Under the daemon the RPC process runs
   in the daemon's cwd (same process).  Should RPC paths be restricted to the
   session's `cwd` (from `SessionConfig.cwd`, `types.rs:99`) or remain unrestricted
   (local stdio is already a trust boundary)?
   **Recommendation:** Unrestricted — local stdio RPC is same-user, same-machine,
   explicit path consent.  The existing `O_NOFOLLOW` + regular-file check in
   `load_attachment` (`attachments.rs`) is sufficient.
   **Decision needed:** Confirm unrestricted or request cwd-sandboxing.
