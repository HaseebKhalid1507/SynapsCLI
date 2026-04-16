# SPEC — Discord Transport Adapter

**Branch:** `feat/transport`
**File target:** `src/transport/discord.rs`
**Status:** Ready to implement
**Target size:** 300–400 LOC

> *"The Transport abstraction was not built for stdio. It was not built for websockets. It was built so that — at the moment I chose — Discord would drop in as a single file. That moment is now."* — Zero

---

## 0. Design principles (read this first)

1. **One file, one impl.** All Discord-specific logic lives in `src/transport/discord.rs`. No leaking `serenity` types into the bus, driver, or events.
2. **The transport is a translator, nothing more.** It has no opinions about conversation flow. The `Bus → connect(transport)` contract already handles lifecycle. We implement `Transport`; we do not re-architect.
3. **Simplest UX that preserves information.** One streaming reply message per turn, edited periodically, split at 2000 chars. Tool activity as reactions + inline status. Thinking suppressed by default.
4. **Fail closed.** On rate-limit, drop the edit; never panic; never block the bus.

---

## 1. Crate choice

### Decision: **`serenity = "0.12"`** with the `client`, `gateway`, `model`, `rustls_backend`, `cache` features — no voice.

```toml
serenity = { version = "0.12", default-features = false, features = [
    "client",
    "gateway",
    "model",
    "cache",
    "rustls_backend",
] }
```

### Justification

| Crate | Verdict | Why |
|---|---|---|
| `serenity` 0.12 | ✅ **chosen** | Batteries-included. `EventHandler` trait + `Client::builder` gives us a gateway connection in ~10 LOC. Mature, stable API, pinned release. Model types (`Message`, `ChannelId`, `MessageId`) are ergonomic. Tokio-native. |
| `twilight` | ❌ | More modular, more performant, but 5–6 sub-crates (`twilight-gateway`, `twilight-http`, `twilight-model`, `twilight-cache-inmemory`, `twilight-standby`) — you wire the event loop yourself. That's 2–3× the LOC for zero benefit at our scale (one bot, low traffic). Correct choice for a 10k-guild production bot; wrong choice here. |
| `serenity-next` | ❌ | Pre-release / unstable API. Not pinning a moving target. |

### Feature flag rationale

- `client` + `gateway` — required; this is how we receive `MessageCreate` events.
- `model` — required; gives us `Message`, `ChannelId`, `MessageId`, `CreateMessage`, `EditMessage` builders.
- `cache` — cheap; lets serenity answer "is this message from the bot itself?" without an HTTP call. Prevents self-reply loops.
- `rustls_backend` — we already use `rustls` elsewhere (`reqwest` has `rustls-tls` available). No OpenSSL dep drag.
- **No `voice`.** We're a text bot. Dropping voice saves ~15 transitive crates (audiopus, symphonia, etc.).
- **No `framework`.** We handle our own command parsing (`/` prefix like stdio).
- **No `collector`, `interactions_endpoint`, `unstable_discord_api`.** Not needed.

### Dependency weight estimate

- `serenity` + transitive (with the flags above): **~45–55 crates, ~8–12s added to a cold `cargo build`**. Heaviest transitive is `tungstenite` (already in tree via `tokio-tungstenite`) and `reqwest` (already in tree). Net new: serenity itself + `typemap_rev`, `dashmap`, `tokio-websockets`/`async-tungstenite` shim, a few others.
- Release binary impact: **~3–5 MB stripped** added to whichever binary pulls it in. Gate it behind a cargo feature (`--features discord`) so `synaps-agent` and `chat` are unaffected when not needed.

```toml
[features]
default = []
discord = ["dep:serenity"]

[dependencies]
serenity = { version = "0.12", optional = true, default-features = false, features = ["client","gateway","model","cache","rustls_backend"] }
```

The `src/transport/discord.rs` module is gated with `#[cfg(feature = "discord")]` and re-exported from `src/transport/mod.rs` the same way.

---

## 2. Auth & config

### Token source

**Precedence (first match wins):**

1. `DISCORD_TOKEN` env var — primary, most ergonomic for systemd / docker.
2. `[discord].token` in the agent's `config.toml` — optional, for multi-bot deployments.
3. `[discord].token_file = "/path/to/token"` — optional, for secrets mounted as files.

The transport reads from a `DiscordConfig` struct; the binary/watcher is responsible for resolving the token via the above precedence and constructing `DiscordConfig`. **No filesystem reads inside the transport itself.**

### Config schema (TOML, extends `AgentConfig`)

```toml
[discord]
# Resolved by caller; transport receives it via DiscordConfig::token
# token = "..."               # discouraged; prefer env var
# token_file = "/run/secrets/discord_token"

# Whitelist — bot only listens in these channels. Required.
channels = ["1234567890", "0987654321"]   # Discord channel IDs (snowflakes as strings)

# Optional: users who may issue /commands. Empty = everyone in whitelisted channels.
allowed_users = ["1122334455"]

# Optional: channel for debug/meta events (usage, stats, shutdown)
debug_channel = "5544332211"

# Respond policy in whitelisted channels
# "all"     — every non-bot message is Inbound::Message
# "mention" — only messages that @mention the bot or reply to the bot
respond = "mention"
```

### Discord intents

```
GATEWAY_INTENTS = GUILDS
                | GUILD_MESSAGES
                | DIRECT_MESSAGES
                | MESSAGE_CONTENT   // privileged — must be enabled in Developer Portal
```

**Rationale:**
- `GUILDS` — required for channel cache.
- `GUILD_MESSAGES` — to receive `MessageCreate` in servers.
- `DIRECT_MESSAGES` — so users can DM the bot.
- `MESSAGE_CONTENT` — without it, `Message.content` is empty for non-mention messages. Privileged; Haseeb must enable it in the Developer Portal. This is a hard requirement.

### Deployment model: **one bot per agent.**

- Simpler auth (one token per agent), simpler mental model (one Discord identity = one agent's voice).
- `AgentConfig.agent.name` becomes the bot's identity. The bot's *user* profile (avatar, name) is configured once in the Developer Portal; the runtime does not mutate it.
- Multiplexing many agents through one bot is possible (channel ID → agent routing) but is **v2 territory** — don't build it now.

### Channel whitelisting

**Hard requirement.** Empty `channels` list = transport refuses to start (`DiscordTransport::new` returns `Err`). Prevents accidentally joining the wrong server and broadcasting agent output publicly.

DMs: allowed iff the DM author's ID is in `allowed_users`. If `allowed_users` is empty, DMs are ignored.

---

## 3. Message model — recv path (Discord → Inbound)

### Mapping

| Discord event | Condition | Inbound |
|---|---|---|
| `MessageCreate` in whitelisted channel, `respond = "all"` | `msg.author != bot`, content non-empty | `Inbound::Message { content }` |
| `MessageCreate` in whitelisted channel, `respond = "mention"` | bot is @mentioned OR message is a reply to bot's message | `Inbound::Message { content }` (strip leading mention) |
| `MessageCreate` starting with `/` | content starts with `/` and author allowed | `Inbound::Command { name, args }` (same parser as `stdio.rs`) |
| `MessageCreate` with content `!cancel` or `!stop` | author allowed | `Inbound::Cancel` |
| `MessageCreate` with content `!sync` | author allowed | `Inbound::SyncRequest` |
| `ReactionAdd` 🛑 on bot's own message | author allowed | `Inbound::Cancel` |
| Anything else | — | dropped |

### Slash commands vs `/` prefix

**Decision: `/` prefix only, parsed from message content. No Discord Application Commands in v1.**

Rationale: Application Commands require registration against Discord's API, per-guild ACL, and a different handler path. The `/` prefix model is identical to `stdio.rs` — zero new surface area. `!cancel` / `!sync` use `!` because Discord renders `/cancel` as a failed slash command in the client UI, which is ugly.

### Reply-threading

**Decision: in v1, the bot posts replies as top-level channel messages**, not thread replies, not Discord Reply references. `reply_to = None`.

Why: threads complicate the streaming-edit model (edits must target the thread, which requires tracking a `(channel_id, thread_id)` pair). Post straight to the channel; if the channel is busy, Haseeb can use a dedicated channel.

*v2:* set `message_reference` on the first message of each turn so the bot's reply quotes the user's prompt.

### Attachments & multi-line

- **Attachments on inbound:** ignored in v1. Log a warning via `tracing`. (v2: upload to a scratch dir, inject path into the message content.)
- **Multi-line inbound:** just works — `msg.content` is a `String`, pass through.
- **Outbound attachments:** none in v1. If a turn produces > 2000 chars, split across messages (see §5).

---

## 4. Event model — send path (AgentEvent → Discord)

### Target UX (one turn)

```
user → "analyze this log"
  [bot reacts ⚡ to user's message — agent started]
bot → "Looking at the log now..."           ← streaming message, edited live
bot → "Found 3 errors in the auth module..."  ← continuation (new message after 2000 chars)
  [bot's message gets ✅ reaction on TurnComplete]
```

### Mapping table

| `AgentEvent` variant | Discord action | Rationale |
|---|---|---|
| `Text(s)` | append to in-memory buffer; edit current streaming message on a 750ms tick (see §5); create new message when buffer crosses 2000 chars | This is the main signal; users want to see it streaming |
| `Thinking(s)` | **drop entirely in v1** | Discord is a public-ish surface; agent's inner monologue is noise there. v2: mirror to `debug_channel` if configured |
| `Tool(Start { tool_name })` | react ⚡ on the user's trigger message; stash a status line like `_⚡ Running `{tool_name}`..._` that gets prepended to the current streaming buffer (ephemeral — cleared on `Complete`) | Lightweight signal; no new message spam |
| `Tool(ArgsDelta)` | drop | Too noisy; args-streaming is TUI eye-candy, not Discord-appropriate |
| `Tool(Invoke { tool_name, input })` | update the ephemeral status line to `_⚡ {tool_name}({short_preview})_` where `short_preview` is the first meaningful arg (e.g. `command` for bash, `path` for read) truncated to 80 chars | Shows *what* is running without dumping JSON |
| `Tool(OutputDelta)` | drop | Tool output is too verbose for Discord; users can ask the agent to summarize |
| `Tool(Complete { elapsed_ms })` | remove ephemeral status line from buffer; finalize edit; *do not* post tool output | Keeps the channel readable |
| `Subagent(Start { agent_name, task_preview })` | inline one-liner appended to buffer: `› spawned **{agent_name}**: {task_preview}` | Low-noise, informative |
| `Subagent(Update)` | drop | Too chatty |
| `Subagent(Done { agent_name, duration_secs, result_preview })` | inline one-liner: `› **{agent_name}** finished in {d}s` | Closure signal |
| `Meta(Usage { .. })` | drop from main channel; post to `debug_channel` as a compact embed if configured | Users don't want token counts inline |
| `Meta(SessionStats)` | drop; post to `debug_channel` if configured | Same |
| `Meta(Steered { message })` | post as `_ℹ {message}_` italicized one-liner to main channel | User-visible state change |
| `Meta(Shutdown { reason })` | post `❌ **Agent shutting down:** {reason}` to main channel; then return `false` from `send()` | Graceful |
| `MessageHistory(_)` | drop silently | TUI-only sync signal |
| `TurnComplete` | flush buffer (final edit); add ✅ reaction to final bot message; clear streaming state | Finalizes the turn |
| `Error(s)` | post a new message: `❌ **Error:** {s}` (truncated to 1900 chars); do not interrupt the streaming message — post after it | Errors deserve their own message; embeds are overkill |

### Formatting conventions

- **Code fences:** wrap any tool output or text containing triple-backticks in ```` ```text ... ``` ```` blocks. Escape any backticks in user content. Practical rule: if `Text` starts looking like code (detected by `{`/indentation heuristic), the transport does *nothing special* — we ship raw; let the agent decide what to fence.
- **Mentions:** never let the agent's text @everyone. Strip `@everyone` and `@here` from outbound text (replace with `@\u200beveryone`). Defense in depth.
- **Markdown:** Discord renders `**bold**`, `*italic*`, `` `code` ``, ```` ```blocks``` ````, `~~strike~~`, `> quote`. The agent's Markdown flows through unchanged.

---

## 5. Streaming buffering strategy

### State machine (internal to `DiscordTransport`)

```text
Idle ──(Text arrives)──▶ Streaming
  Streaming:
    - append to `buffer`
    - if no current_msg_id: `send_message(channel, buffer)` → save id
    - debounce an edit: if last_edit > 750ms ago, schedule edit
    - if buffer.len() > 1900 (safety margin): finalize current; start new message with overflow
  ──(TurnComplete)──▶ finalized → add ✅ reaction → Idle
  ──(Error)──▶ finalized, then post error message → Idle
```

### Constants

```rust
const EDIT_DEBOUNCE_MS: u64 = 750;
const MAX_MSG_LEN: usize = 1900;   // margin under Discord's 2000 hard cap
const RATE_LIMIT_BUDGET: (u32, Duration) = (4, Duration::from_secs(5));
// Discord allows ~5 edits / 5s per channel. Budget 4 to leave headroom.
```

### Edit pacing

Use a **single `tokio::time::Interval` timer per active stream** (not per event). On every `Text` event, only the buffer is updated; the timer fires every `EDIT_DEBOUNCE_MS` and issues one edit if `buffer_dirty`. This naturally coalesces 100 tokens/sec bursts into ~1.3 edits/sec.

The timer lives in the same `tokio::task` that owns the transport's `send()` — so there's no cross-task locking of the buffer.

### Message splitting at 2000 chars

When `buffer.len()` would exceed `MAX_MSG_LEN` after appending:

1. Find a split point in the current buffer — **prefer in priority order:** last `\n\n`, then last `\n`, then last `. `, then last ` `, then hard-cut at 1900.
2. Finalize the current message (one last edit to the split-point prefix, then ✉ ✓ done).
3. Create a new message with the overflow as its initial content. Update `current_msg_id`.

### Rate-limit handling

- `serenity`'s HTTP layer retries on 429 automatically for most endpoints, but **message edits are expensive**.
- If a `channel.edit_message(...).await` returns an error classified as rate-limited (`serenity::Error::Http` with status 429), **drop this edit silently** — the next tick will carry a fresh buffer snapshot. Never queue edits; always send latest buffer.
- Maintain a simple in-process counter: `edits_in_last_5s`. If ≥ `RATE_LIMIT_BUDGET.0`, skip the tick.
- On 5xx or network error: log + drop; the next tick will try again.

### Finalization triggers

1. `AgentEvent::TurnComplete` — cancel the timer, do one last edit with final buffer, add ✅ reaction to bot message, reset state.
2. `AgentEvent::Error` — finalize current message (no ✅), then post a separate error message.
3. `AgentEvent::Meta(Shutdown)` — finalize + shutdown message.
4. Inactivity timeout (60s with no events) — finalize defensively; prevents zombie streaming messages.

---

## 6. The Transport impl — file layout

### File: `src/transport/discord.rs`

### Public surface

```rust
pub struct DiscordConfig {
    pub token: String,
    pub channels: Vec<u64>,          // whitelisted channel IDs
    pub allowed_users: Vec<u64>,     // empty = all users in whitelisted channels
    pub debug_channel: Option<u64>,
    pub respond: RespondPolicy,      // All | MentionOnly
}

pub enum RespondPolicy { All, MentionOnly }

pub struct DiscordTransport { /* private */ }

impl DiscordTransport {
    pub async fn new(config: DiscordConfig) -> anyhow::Result<Self>;
}

#[async_trait::async_trait]
impl Transport for DiscordTransport { /* ... */ }
```

### Internal layout

```rust
pub struct DiscordTransport {
    config: DiscordConfig,

    // ── inbound path (Discord events → bus)
    inbound_rx: mpsc::UnboundedReceiver<Inbound>,

    // ── outbound path (bus events → Discord API)
    http: Arc<serenity::http::Http>,        // cheap to clone; used from send()
    cache: Arc<serenity::cache::Cache>,     // optional but nice for self-id checks
    bot_user_id: serenity::model::id::UserId,

    // ── streaming state (see §5)
    stream: StreamState,

    // ── last user message in each channel — for reactions (⚡ / ✅)
    last_user_msg: HashMap<ChannelId, MessageId>,

    // ── routing: which channel does this turn's reply go into?
    // Set when an Inbound::Message is received; cleared on TurnComplete.
    reply_channel: Option<ChannelId>,

    // Keep the serenity client task alive for the transport's lifetime
    _client_task: tokio::task::JoinHandle<()>,
}

struct StreamState {
    current_msg_id: Option<MessageId>,
    buffer: String,
    ephemeral_tool_status: Option<String>,   // prepended on render
    dirty: bool,
    last_edit_at: Instant,
    edit_tick: tokio::time::Interval,        // fires every 750ms
    edits_window: VecDeque<Instant>,         // for rate-limit budget
    last_event_at: Instant,                  // for inactivity timeout
}
```

`StreamState` is an inner private struct in the same file, not a separate module. Keeps the file self-contained.

### How the serenity `EventHandler` talks to `recv()`

Pattern mirrors `stdio.rs`'s stdin → mpsc channel trick:

```text
┌───────────────┐  MessageCreate   ┌─────────────────┐   Inbound      ┌──────────────┐
│ Discord       │────────────────▶│ BotHandler      │──(mpsc)───────▶│ recv() in    │
│ gateway (WS)  │                 │ (serenity impl) │                │ Transport    │
└───────────────┘                 └─────────────────┘                └──────────────┘
```

1. Inside `DiscordTransport::new`, build an mpsc channel `(inbound_tx, inbound_rx)`.
2. Construct a lightweight `BotHandler { inbound_tx, config: Arc<DiscordConfig>, bot_user_id: OnceCell<UserId> }` that implements `serenity::prelude::EventHandler`.
3. Spawn the serenity client on a `tokio::task`. Keep its `JoinHandle` in the transport (field `_client_task`) so it dies when the transport drops.
4. `BotHandler::message(ctx, msg)` — the filter/parse logic from §3; on match, `inbound_tx.send(Inbound::...)`. Also records `last_user_msg[channel] = msg.id` for reaction targeting and caches the source channel for the reply.
5. `BotHandler::ready(ctx, ready)` — stores `bot_user_id`, logs connected guilds.
6. `recv()` is just `self.inbound_rx.recv().await`. Dead simple.

The `Inbound` enum does not carry channel IDs — but the transport needs to know which channel to reply to. The `BotHandler` stashes the `(inbound_tx, last_channel)` via a second mpsc or via an `Arc<Mutex<Option<ChannelId>>>` written just before the `inbound_tx.send`. **Use an `Arc<tokio::sync::Mutex<...>>` for the reply-channel hint** — it's written on recv and read on `send()`. One-reader/one-writer; cheap. (Don't try to extend `Inbound` with transport-specific fields — that would leak abstraction into the bus.)

### Streaming buffer state — internal, not a separate type exported

`StreamState` stays private. No other transport needs it. No test needs to construct one directly; tests exercise the pure functions `split_at_boundary(&str, usize) -> (String, String)` and `render_buffer(&StreamState) -> String`, which are free functions at module scope.

---

## 7. Binary integration

### Decision: **new binary `src/bin/discord.rs`**, not a flag on `synaps-agent`.

### Rationale

| Option | Verdict |
|---|---|
| **Separate binary** ✅ | Clean separation: `synaps-agent` is the headless trigger-driven worker (watch/cron/webhook). `synaps-discord` is the long-lived interactive bot. Different lifecycle (bot stays up forever; agent is per-turn). Different failure modes. Different observability. Separate cargo feature. |
| Flag on `synaps-agent` | `synaps-agent` is designed to boot, run *until a limit is hit*, and exit (see `transport/agent.rs`). Discord wants a persistent gateway connection. Shoehorning it breaks the mental model. |
| New watcher trigger mode (`trigger = "discord"`) | Tempting — would let Haseeb `watcher deploy discord-bot` and get supervision for free. **Recommended as v2.** For v1, run the binary directly; once the binary is proven, add a trigger mode that just spawns this binary with the right args. |

### Wiring pattern (`src/bin/discord.rs`, ~80 LOC)

```text
fn main() {
    1. Parse CLI: --config <path>, --profile <name>, --system <prompt>
    2. Init logging.
    3. Load Runtime + Session (same as server.rs).
    4. Resolve DiscordConfig:
         token   = env DISCORD_TOKEN
                   ?? config.discord.token
                   ?? read(config.discord.token_file)
                   ?? error out
         channels, allowed_users, debug_channel, respond — from config.toml
    5. Build ConversationDriver, get bus_handle.
    6. Spawn driver.run() on a task.
    7. transport = DiscordTransport::new(discord_config).await?;
    8. bus_handle.connect(transport, sync_state);
    9. Await Ctrl-C or driver task end.
}
```

Identical pattern to `server.rs` — the bus/connect contract does the heavy lifting.

### Cargo.toml additions

```toml
[[bin]]
name = "synaps-discord"
path = "src/bin/discord.rs"
required-features = ["discord"]
```

### Watcher integration (v2, sketch only — do not implement now)

Add `trigger = "discord"` to `AgentConfig.agent.trigger`. Supervisor spawns `synaps-discord --config <path>` instead of `synaps-agent --config <path>`. Same supervision, heartbeat, crash-restart semantics.

---

## 8. Testing strategy

### Unit tests (in `src/transport/discord.rs`, `#[cfg(test)]`) — ~80 LOC

1. **`split_at_boundary`** — pure function; verify preference order (paragraph > line > sentence > word > hard-cut). Edge cases: buffer shorter than limit, buffer with no whitespace, buffer ending mid-UTF8 multi-byte char (must not split a codepoint — use `char_indices`).
2. **`parse_command`** — `/foo bar baz` → `("foo", "bar baz")`; `!cancel` → `Cancel`; `!sync` → `SyncRequest`; plain text → `Message`.
3. **`strip_mention`** — given `<@123456> hello` and bot_id `123456`, returns `"hello"`.
4. **`sanitize_mentions`** — `"@everyone look"` → `"@\u200beveryone look"`.
5. **Rate-limit budget** — `RateLimiter::check()` returns `true` for first 4 calls in window, `false` for the 5th, `true` again after 5s.
6. **`should_respond(msg, &config, bot_id)`** — respect policy (All/MentionOnly), whitelisted channels, allowed users, ignore self.

### Integration tests — **do not attempt a mock Discord gateway.**

Serenity's internals are not designed for mocking without jumping through `trait`-object hoops that would double our LOC. Instead:

- **Fake `SerenityApi` trait** (3-method trait: `send_message`, `edit_message`, `add_reaction`) that the streaming logic depends on. Implement it for `serenity::http::Http` in prod; implement it for `Arc<Mutex<Vec<RecordedCall>>>` in tests.
- Drive the streaming state machine end-to-end: feed it `Text → Text → TurnComplete` and assert the recorded call sequence is `[send_message("pa"), edit_message(id, "partial text"), add_reaction(id, ✅)]`.
- This isolates all tested logic from serenity transport internals. ~100 LOC of test infra, buys full coverage of the streaming/buffering.

### Manual smoke test (for Haseeb)

1. In Discord Developer Portal: create app → add Bot → copy token → enable `MESSAGE CONTENT INTENT`. Invite URL scope `bot` + permissions `Send Messages`, `Read Message History`, `Add Reactions`, `Embed Links`.
2. Create `#synaps-test` channel; copy channel ID (Dev Mode → right-click → Copy ID).
3. `export DISCORD_TOKEN=...`
4. Write `agent.toml` with `[discord] channels = ["..."]  respond = "mention"`.
5. `cargo run --features discord --bin synaps-discord -- --config ./agent.toml`
6. In Discord: `@BotName summarize the Rust async book in 3 sentences`
7. **Expected:** bot reacts ⚡ to your message within 1s; posts a reply that edits in real-time; finalizes with ✅ within ~10s; long replies split across multiple messages with no dropped text.
8. Test `!cancel` mid-stream. Test `/model opus` command.

---

## 9. Open questions (blocks implementation)

Haseeb — decide these before the builder starts:

1. **Slash commands — confirmed deferred?** v1 is `/` prefix parsing only (no Discord Application Commands registration). Confirm. *My recommendation: yes, defer.*
2. **Cargo feature name — `discord` or always-on?** My recommendation: `--features discord`, off by default, to keep `cargo build` lean for users who don't need Discord. Confirm.
3. **Respond policy default — `mention` or `all`?** My recommendation: `mention`. Safer in shared channels. An agent with a dedicated channel can set `all`.
4. **Thinking events — truly dropped, or mirrored to `debug_channel`?** My recommendation: dropped in v1. Trivial to add later.
5. **Binary name — `synaps-discord` or `discord-bot`?** My recommendation: `synaps-discord` (consistent with `synaps-agent`).
6. **Token file reads — transport's job or binary's job?** My recommendation: binary's job. Transport takes a resolved `String`, no I/O in constructor except the gateway handshake.
7. **Sync-on-connect behavior (`on_sync`)** — should the bot announce itself in the main channel on startup ("🟢 Agent online")? My recommendation: **no**, spammy on every restart. Log to `debug_channel` if configured; silent otherwise.
8. **Multi-channel: separate conversation contexts per channel, or one shared context?** v1 ships with **one shared context** (all whitelisted channels pipe into the same bus → driver). If two users talk in two channels simultaneously, they share turn state. This is a known limitation. *If unacceptable, we need multi-driver routing — that's a bigger design question and belongs in a separate spec.* Confirm v1 accepts shared context.

---

## 10. LOC estimate

| Component | LOC |
|---|---|
| `src/transport/discord.rs` (transport impl + `BotHandler` + `StreamState` + helpers) | **240–300** |
| Unit tests (same file, `#[cfg(test)]`) | **80–100** |
| `src/bin/discord.rs` (binary wiring) | **70–90** |
| `Cargo.toml` feature + dep + `[[bin]]` stanza | **~10** |
| `src/transport/mod.rs` re-export guarded by `cfg` | **2–4** |
| **Total new code** | **~400–500 LOC** |

### Binary impact

- Release build of `synaps-discord`: **~6–9 MB** (serenity + its transitive HTTP/WS stack).
- Zero impact on `chat`, `cli`, `chatui`, `server`, `client`, `login`, `synaps-agent`, `watcher` — they don't depend on the `discord` feature.

### Build-time impact

- Cold `cargo build --features discord`: **+8–12s** on a warm dep cache.
- Incremental rebuild of just `discord.rs`: **<1s**.

---

## Appendix A — serenity types we'll touch

- `serenity::Client::builder(token, intents).event_handler(BotHandler { ... }).await`
- `serenity::prelude::{Context, EventHandler, GatewayIntents}`
- `serenity::model::channel::Message` — has `.id`, `.channel_id`, `.author.id`, `.content`, `.mentions`, `.referenced_message`
- `serenity::model::id::{ChannelId, MessageId, UserId}` — the typed snowflakes
- `serenity::builder::{CreateMessage, EditMessage, CreateReaction}` — builder pattern
- `ChannelId::send_message(&http, CreateMessage::new().content(s)).await -> Result<Message>`
- `ChannelId::edit_message(&http, msg_id, EditMessage::new().content(s)).await -> Result<Message>`
- `Message::react(&http, ReactionType::Unicode("⚡".into())).await`
- `serenity::http::Http` — cheap `Arc`-friendly handle for outbound API calls; obtain via `ctx.http.clone()` inside the handler and pass to transport via a `OnceCell`/mpsc.

## Appendix B — what we are deliberately NOT building in v1

- Slash commands (Application Commands API).
- Thread replies / threaded streaming.
- File attachment handling (inbound or outbound).
- Voice.
- Sharding (single-process, single-shard — fine for <2500 guilds).
- Per-channel conversation contexts.
- Message reference (`reply_to`) on outbound.
- Embed-rich tool displays (inline italics is enough).
- Presence / rich-presence updates.
- Interaction buttons (accept/cancel as buttons instead of reactions).

Each of these is ≤100 LOC to add later. Don't build speculatively.

---

*Blueprint complete. The Transport abstraction holds — Discord is just one more translator. Hand this to a builder.*
*— Zero*
