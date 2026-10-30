# Spec: Multi-client sessions: one user, many clients

**Status:** v1.1, accepted direction (owner, 2026-10-24); amended after item 3 was built (no `SessionQuery::Clients`; `ClientInfo` carries `mode`). Verified against dev `978a1890` + PR #296 (`/takeover`).
**Scope:** the daemon socket (`daemon.sock`), the session actor's input rules, the TUI, `synaps attach`, `synaps daemon sessions`.
**Out of scope:** event-inbox sockets (`s-<hash>.sock`, their own trust model); remote access; several people sharing one session (§9).

---

## 1. Product position

Every client attached to a session is **the same user**. Two terminal tabs, an SSH session from the laptop, a sidebar, a status-bar widget, a dashboard and a Discord bridge are all you, in different places. Synaps should behave like tmux and Jupyter, where every attached view can type, and not like a lock where every tab but one is read-only.

So:

1. **Same user, enforced.** The daemon refuses any connection from another OS user (§3).
2. **Clients have names.** `terminal`, `discord`, `dashboard`, `sidebar`, `status`. A name is a label and not a login (§4).
3. **Input follows the keyboard.** Any client that can drive may type. The client you typed in last is the *active* one. There is nothing to refuse and nothing to take over (§6).
4. **Clients pick a role:** drive, observe or status (§5).
5. **A client can lower its own powers** (a Discord bridge never approves a bash prompt), and the daemon enforces this (§7).

What it costs: the single-owner rule goes away by default. It protected nothing in case 1 below, because it's the same person on both ends, and it caused real friction: a stale tab from hours ago owned input, so every new tab was read-only. `client_input = exclusive` keeps the old rule for anyone who wants it, with `/takeover` (#296) as its explicit handoff.

### How people use this, most common first

1. One person in several places: tabs, SSH plus local, closing the lid and reattaching, later web or phone.
2. Watching: a pane that follows the agent while you work elsewhere (`--observe`).
3. Integrations: a git hook, an editor plugin, `synaps attach -s "run the tests"`, a Discord bridge, a dashboard.
4. Several people: rare, needs remote access and identity, and is designed separately (§9).

---

## 2. Today (facts this spec builds on)

- `daemon.sock` is mode `0600` inside a `0700` dir (`daemon/listener.rs:21`, `daemon/mod.rs:373`). `accept_loop` never checks who connected (`listener.rs:43`).
- `Hello.client: ClientMeta { kind, terminal, instance, history, tail_items, capabilities }` (`session/types.rs:389`). `ClientKind = Tui | Chat | Attach | Rpc | Server | Test`.
- `AttachMode = Mirror | Takeover | Observe`. Mirror takes input only if nobody owns it. Takeover always takes it. Observe never does.
- The actor has one `input_owner: Option<ClientId>`. `is_input_command` (`actor.rs:357`) lists Submit, SubmitPrepared, Steer, SubagentSteer, Cancel, Set, Compact, NewSession, EngineCommand, PluginCommand, Resume, KeepWarm, DriverStart, Checkpoint and End{ClientQuit}. From anyone else these get `Refused` with "input owned by client #N" (`actor.rs:4902`).
- Confirm prompts: an Observe client can't answer them at all, and only the owner may pick "Allow all" (`confirm_answer_refusal`, `actor.rs:4123`). Every other answer is first-wins.
- When the owner detaches, input goes to the non-Observe client with the lowest id (`actor.rs:4563`).
- The owner also gates `background.set_inbox_accepting(input_owner.is_some())` and the reload reconnect order (`ClientReconnect.was_owner`).
- `AttachSnapshot.clients: Vec<(ClientId, ClientKind)>` and `input_owner`. Events: `ClientJoined { client, kind }`, `ClientLeft`, `InputOwnerChanged { from, to, reason }`. `OwnerChangeReason = Attach | Takeover | OwnerDetached | Reload`, with **no `#[serde(other)]`**: a new variant breaks older clients unless gated by `ClientCapabilities`.
- `Presence { clients, input_owner, awaiting_input }` feeds `synaps daemon sessions` ("clients=N owner=#M").

---

## 3. Same user, enforced

On every accepted `daemon.sock` connection, before reading `Hello`:

- Read the peer's credentials (`tokio::net::UnixStream::peer_cred()`: `SO_PEERCRED` on Linux, `getpeereid` on macOS/BSD).
- If `uid != geteuid()`, close the stream without sending a frame and log `warn!(peer_uid, "daemon: refused connection from another user")`.
- If the credentials can't be read, refuse the same way (fail closed).
- This applies to root as well. `sudo synaps --attach` would also resolve a different home directory, so refusing it is correct.

The file permissions already enforce this. The check keeps it true when someone loosens the permissions, points `SYNAPS_BASE_DIR` at a shared path, or a future listener forgets to chmod. Windows (#277) has no equivalent on AF_UNIX, so it stays permission-only there and gets a code comment.

---

## 4. Client names

`ClientMeta.name: Option<String>`: additive, `serde(default, skip_serializing_if = "Option::is_none")`. An older daemon ignores it.

**Set from:** `--as <name>` on `synaps --attach` and `synaps attach` (`synaps chat` is in-process and never reaches the daemon); `SYNAPS_CLIENT_NAME` (already stripped from the forwarded env, since `SYNAPS_CLIENT_*` is); the handshake for rpc and server clients.

**Valid:** 1–32 chars, ASCII letters, digits, `-` and `_`, starting with a letter or digit. The client validates before connecting and prints a clear error. The daemon validates again and answers an invalid name with `DaemonFrame::Error { "invalid client name …" }` + `Bye`.

**Resolved by the daemon, per session:**
- With no name, a client gets its kind's name: `tui`, `chat`, `attach`, `rpc`, `server`, `test`.
- A name already taken in this session gets the lowest free suffix: `tui-2`, `tui-3`. Matching is case-insensitive.
- The resolved name is what every client sees. The joiner learns its own from its snapshot.

**Carried in:**
- `ClientJoined { client, kind, name, mode }`. On the wire `name` is `serde(default)` and `mode` is an `Option`, read as `Mirror` when absent.
- `AttachSnapshot.roster: Vec<ClientInfo>` (new field, `serde(default)`). The old `clients` tuple list stays for older clients.
- `ClientInfo { id, name, kind, mode, attached_at_ms }`. The role follows from `mode`: `Observe` is observe, and `Mirror`/`Takeover` are drive.
- `Roster`: the client-side copy, kept by every transport from the snapshot plus `ClientJoined`/`ClientLeft` (`ClientTransport::roster()`).
- `Presence` and `SessionMeta` gain `roster: Vec<ClientInfo>` (`SessionMeta`'s is `serde(default)`).

**Shown in:**
- `/clients` (new TUI command, works mid-stream): one row per client with name (#id), kind, drive or observe, `owns input`, `you` and how long it has been attached. It reads the transport's roster.
  - There is **no new query**. An older daemon answers a frame it can't decode with "malformed frame" and never sends a `QueryResult`, so a `SessionQuery::Clients` would hang `/clients` against it.
- Toasts and notices: "input moved to discord", "input taken over by terminal-2", "input owned by terminal (#3) · /takeover to take it".
- Refusal reasons: "input owned by terminal (#3)" (`#N` stays for scripts that grep it).
- `synaps daemon sessions`: `clients=terminal*,discord,dashboard(observe)`, where `*` marks the active client.

---

## 5. Roles

A client picks its role at attach time. The role never changes during the attachment.

| role | attach | transcript | can send | for |
|---|---|---|---|---|
| **drive** | `Mirror` / `Takeover` (default) | full | input commands (within its limits, §7) | terminal, sidebar, discord, scripts |
| **observe** | `Observe` (`--observe`) | full | nothing that changes the session; may answer non-confirm prompts as today | dashboard, a watching pane |
| **status** | `Status` (`--status`, new) | **none** | nothing but `Detach` and `Query{Status}` | status-bar widgets, menu-bar apps |

**Status clients:**
- `AttachMode::Status` is a new variant. An older daemon fails to decode the `Attach` frame, and the client reports "daemon too old for --status".
- The snapshot is trimmed: empty `conversation`, empty `replay`, `display_tail: None`. The rest is kept: meta, view, `streaming`, the pending-prompt **count** and the roster.
- The connection's writer drops every event except state-level ones: turn started and ended, usage and cost, prompt opened and closed, `InputOwnerChanged`, `ClientJoined` and `ClientLeft`, `Cleared`, park/wake, the session ending, and model `SettingChanged`. The exact list goes in `conn.rs` with a test per kept and dropped family.
- A status bar running all day then costs roughly a frame per turn, not the token stream.
- Status clients never count as attached for park decisions, so a widget can't keep a session out of park forever. Observe clients keep their current behaviour.

---

## 6. Input modes and the active client

A session setting `client_input`, `shared | exclusive`:
- It starts from config: `client_input = shared` is the config default.
- `/settings` changes it live through `SessionSetting::ClientInput`, echoed in `SettingChanged`. That new nested variant is sent only to clients declaring the `client_input` capability. Older clients just don't hear about the change.
- **Wire default ≠ config default.** A snapshot from an older daemon has no `client_input` field, and a client must read that as `exclusive`, which was the old behaviour.

### The active client

Both modes keep `input_owner`, renamed *active client* in docs and UI. Wire names stay. It is used only where something needs a single place:
- the reload reconnect order (`was_owner`),
- inbox accepting,
- in `exclusive` mode, who may drive.

### `shared` (default)

- **Drive clients may send every input command**, subject to §7. Observe and status clients are refused with "this client is read-only (observe)", not "input owned by …".
- **Activity moves the active marker.**
  - Activity means every input command except `KeepWarm`. That one is sent automatically on attach with `--keep-warm`, so it doesn't count.
  - An `Answer` and a `TakeInput` also count.
  - Activity from a drive client that isn't active makes it active *before* the command runs.
  - Every client gets `InputOwnerChanged { reason: Input }`. `Input` is a new variant, gated by a `owner_reason_input` capability; older clients get `Takeover`.
- **"Allow all"**: because an answer is activity, the answering drive client is active by the time the existing "only the owner" check runs. In shared mode, any drive client can pick it, within its limits, with no new rule.
- **Promotion on detach:** the active client goes to the drive client with the most recent input, tracked by the actor per client, not the lowest id. Ties go to the lowest id. With no input yet, it's the lowest id, as today. This way a script that sends one prompt and leaves hands input back to the tab you were typing in.
- **Attaching never moves the active marker** if someone already holds it, as in Mirror today. A new tab becomes active when you type in it. `--takeover` still makes the joiner active immediately.
- **Concurrency is ordinary:** two tabs submitting at once is one user submitting twice. The actor already serialises commands, and a submit during a stream follows the existing queue and steer rules.

### `exclusive`

Exactly today's behaviour, with names in the texts. `/takeover` (#296) is the explicit handoff.

### TUI behaviour

- **Shared mode is quiet.** There's no toast when the active marker moves. A header state block shows `⧉ 2` only while more than one client is attached; the count excludes status clients. `/clients` lists them. The read-only attach notice and the `/takeover` hint appear only in exclusive mode.
- **Exclusive mode** keeps today's toasts, with names in them.
- A drive TUI that isn't active **keeps its Allow-all button** in shared mode. `prompt_bridge.set_owns_input` becomes "may drive": `true` for any drive client in shared mode, `true` only for the owner in exclusive mode.
- **Drafts are per client.** Typing in one tab never touches another tab's input box. A prompt submitted elsewhere shows up in every tab's transcript as a user message, as it already does for observers.
- `/takeover` in shared mode makes this client active and says "input is shared; this client is now active". That's useful only for the reconnect order, so the help text says so.

---

## 7. Limits: a client lowers its own powers

`ClientMeta.limits: Option<ClientLimits>`: an allow-list of powers. If absent, the client has every power its role allows. A declared list can only remove powers; a role never gains one.

| power | covers |
|---|---|
| `prompt` | Submit, SubmitPrepared, Steer, SubagentSteer, Checkpoint, answers to non-confirm prompts |
| `cancel` | Cancel |
| `approve` | a yes or Allow-all answer to a confirm prompt. **Deny is always allowed.** |
| `allow_all` | the "Allow all" answer (needs `approve` too) |
| `settings` | Set, KeepWarm |
| `session` | Compact, NewSession, Resume, EngineCommand (incl. `/close`), PluginCommand, DriverStart, End{ClientQuit} |
| `take_input` | TakeInput |

A command outside the limits gets `Refused { reason: "not allowed for client 'discord' (limits)" }`. An outside confirm answer gets `Refused`, and the prompt stays pending for another client.

**Presets** live client-side, as a convenience in the bridge SDK and in docs:
- `bridge` = `prompt, cancel` (what a Discord or Slack bridge gets).
- `script` = `prompt, cancel, settings`.

**Fail closed against older daemons.** An older daemon ignores `limits`, and a bridge that declared limits would silently get every power. So `Welcome` gains `features: Vec<String>` (`serde(default)`), and a daemon that enforces limits lists `"client_limits"`. The client library refuses to attach when it declared limits and the feature is missing.

**What limits are not:** a sandbox. Any process running as you can connect without limits. Limits protect you from mistakes in a well-behaved bridge, such as a forwarding bug or a channel member typing "yes". The bridge still decides who on Discord may talk to it.

---

## 8. Compatibility

| new field / variant | older peer | handling |
|---|---|---|
| `ClientMeta.name`, `.limits` | older daemon ignores | name: harmless. limits: `Welcome.features` gate (§7) |
| `ClientJoined.name`, `AttachSnapshot.roster` / `.client_input` | older client ignores | `serde(default)`. Missing `client_input` = exclusive |
| `OwnerChangeReason::Input` | older client would fail the frame | capability `owner_reason_input`; older clients get `Takeover` |
| `SessionSetting::ClientInput` in `SettingChanged` | same | capability `client_input` |
| `AttachMode::Status` | older daemon fails the Attach decode | client error "daemon too old for --status" |
| peer-uid check | n/a | same-user clients see nothing new |

---

## 9. Not now: several people

Real collaboration needs owner, collaborator and viewer roles, authentication, remote transport, per-person attribution in the transcript, and an answer to "whose credentials pay for the turn". That's a product of its own, designed when Synaps goes remote. Nothing here blocks it: names, roles and limits are what a future identity layer would assign, instead of a client declaring them.

---

## 10. Delivery

Each item is one PR to `dev`, with tests that fail before the change.

1. **This spec** (docs only).
2. **Same-user check** (§3). `daemon/listener.rs` or `conn.rs`. Test: accepting a stream whose peer uid differs is refused, using a seam over `peer_cred` since tests can't switch uid.
3. **Client names + roster** (§4). `ClientMeta.name`, validation, per-session resolution, `ClientJoined.name`, `roster`, `Presence`/`SessionMeta` roster, `/clients`, names in toasts, refusals and `daemon sessions`, `--as` and `SYNAPS_CLIENT_NAME`.
4. **Shared input** (§6). Builds on #296 and item 3. `client_input` config and setting, the activity rule, `OwnerChangeReason::Input` + capability, promotion by recency, the TUI's "may drive", the quiet shared mode and the header block.
5. **Status role** (§5).
6. **Limits** (§7), with `Welcome.features`.

Items 2 and 3 are independent of #296. Item 4 needs #296 merged, or is stacked on it.
